use crate::utils::units::mib_to_bytes;

use crate::js_worker::module_loader;
use anyhow::{anyhow, Error};
use deno_core::error::AnyError;
use deno_core::url::Url;
use deno_core::{located_script_name, serde_v8, JsRuntime, ModuleId, RuntimeOptions};
use import_map::{parse_from_json, ImportMap, ImportMapDiagnostic};
use log::{debug, warn};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;
use std::{fmt, fs};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use urlencoding::decode;

use crate::{errors_rt, snapshot};
use module_loader::DefaultModuleLoader;
use sb_core::http_start::sb_core_http;
use sb_core::net::sb_core_net;
use sb_core::permissions::{sb_core_permissions, Permissions};
use sb_core::runtime::sb_core_runtime;
use sb_core::sb_core_main_js;
use sb_env::sb_env as sb_env_op;
use sb_worker_context::essentials::{
    EventWorkerRuntimeOpts, UserWorkerMsgs, WorkerContextInitOpts, WorkerRuntimeOpts,
};
use sb_worker_context::events::WorkerEvents;
use sb_workers::events::sb_user_event_worker;
use sb_workers::sb_user_workers;

fn load_import_map(maybe_path: Option<String>) -> Result<Option<ImportMap>, Error> {
    if let Some(path_str) = maybe_path {
        let json_str;
        let base_url;

        // check if the path is a data URI (prefixed with data:)
        // the data URI takes the following format
        // data:{encodeURIComponent(mport_map.json)?{encodeURIComponent(base_path)}
        if path_str.starts_with("data:") {
            let data_uri = Url::parse(&path_str)?;
            json_str = decode(data_uri.path())?.into_owned();
            base_url =
                Url::from_directory_path(decode(data_uri.query().unwrap_or(""))?.into_owned())
                    .map_err(|_| anyhow!("invalid import map base url"))?;
        } else {
            let path = Path::new(&path_str);
            let abs_path = std::env::current_dir().map(|p| p.join(path))?;
            json_str = fs::read_to_string(abs_path.clone())?;
            base_url = Url::from_directory_path(abs_path.parent().unwrap())
                .map_err(|_| anyhow!("invalid import map base url"))?;
        }

        let result = parse_from_json(&base_url, json_str.as_str())?;
        print_import_map_diagnostics(&result.diagnostics);
        Ok(Some(result.import_map))
    } else {
        Ok(None)
    }
}

fn print_import_map_diagnostics(diagnostics: &[ImportMapDiagnostic]) {
    if !diagnostics.is_empty() {
        warn!(
            "Import map diagnostics:\n{}",
            diagnostics
                .iter()
                .map(|d| format!("  - {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

pub struct DenoRuntimeError(Error);

impl PartialEq for DenoRuntimeError {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_string() == other.0.to_string()
    }
}

impl fmt::Debug for DenoRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[Js Error] {}", self.0)
    }
}

fn get_error_class_name(e: &AnyError) -> &'static str {
    errors_rt::get_error_class_name(e).unwrap_or("Error")
}

pub struct DenoRuntime {
    pub js_runtime: JsRuntime,
    pub main_module_id: ModuleId,
    pub is_user_runtime: bool,
    pub is_event_worker: bool, // TODO: make this a method
    pub env_vars: HashMap<String, String>,
    pub conf: WorkerRuntimeOpts,
}

impl DenoRuntime {
    pub async fn new(
        opts: WorkerContextInitOpts,
        event_manager_opts: Option<EventWorkerRuntimeOpts>,
    ) -> Result<Self, Error> {
        let mut maybe_events_msg_tx: Option<mpsc::UnboundedSender<WorkerEvents>> = None;

        let WorkerContextInitOpts {
            service_path,
            no_module_cache,
            import_map_path,
            env_vars,
            conf,
        } = opts;

        let (is_user_runtime, is_event_worker, user_rt_opts) = match conf.clone() {
            WorkerRuntimeOpts::UserWorker(conf) => (true, false, Some(conf)),
            WorkerRuntimeOpts::MainWorker(_conf) => (false, false, None),
            WorkerRuntimeOpts::EventsWorker => (false, true, None),
        };

        if is_user_runtime {
            if let WorkerRuntimeOpts::UserWorker(conf) = conf.clone() {
                if let Some(events_msg_tx) = conf.events_msg_tx {
                    maybe_events_msg_tx = Some(events_msg_tx)
                }
            }
        }

        let user_agent = "supabase-edge-runtime".to_string();
        let base_url =
            Url::from_directory_path(std::env::current_dir().map(|p| p.join(&service_path))?)
                .unwrap();
        // TODO: check for other potential main paths (eg: index.js, index.tsx)
        let main_module_url = base_url.join("index.ts")?;

        // Note: this will load Mozilla's CAs (we may also need to support system certs)
        let root_cert_store = deno_tls::create_default_root_cert_store();

        let extensions = vec![
            sb_core_permissions::init_ops(),
            deno_webidl::deno_webidl::init_ops(),
            deno_console::deno_console::init_ops(),
            deno_url::deno_url::init_ops(),
            deno_web::deno_web::init_ops::<Permissions>(deno_web::BlobStore::default(), None),
            deno_fetch::deno_fetch::init_ops::<Permissions>(deno_fetch::Options {
                user_agent: user_agent.clone(),
                root_cert_store: Some(root_cert_store.clone()),
                ..Default::default()
            }),
            deno_websocket::deno_websocket::init_ops::<Permissions>(
                user_agent,
                Some(root_cert_store.clone()),
                None,
            ),
            // TODO: support providing a custom seed for crypto
            deno_crypto::deno_crypto::init_ops(None),
            deno_net::deno_net::init_ops::<Permissions>(Some(root_cert_store), false, None),
            deno_tls::deno_tls::init_ops(),
            deno_http::deno_http::init_ops(),
            deno_io::deno_io::init_ops(Some(Default::default())),
            deno_fs::deno_fs::init_ops::<Permissions>(false),
            sb_env_op::init_ops(),
            sb_os::sb_os::init_ops(),
            sb_user_workers::init_ops(),
            sb_user_event_worker::init_ops(),
            sb_core_main_js::init_ops(),
            sb_core_net::init_ops(),
            sb_core_http::init_ops(),
            sb_core_runtime::init_ops(Some(main_module_url.clone())),
        ];

        let import_map = load_import_map(import_map_path)?;
        let module_loader = DefaultModuleLoader::new(import_map, no_module_cache)?;

        let ret = deno_core::v8_set_flags(vec!["IGNORED".to_string()]);
        debug!("v8 flags unrecognized {:?}", ret);

        let mut js_runtime = JsRuntime::new(RuntimeOptions {
            extensions,
            module_loader: Some(Rc::new(module_loader)),
            is_main: true,
            create_params: {
                if is_user_runtime {
                    Some(deno_core::v8::CreateParams::default().heap_limits(
                        mib_to_bytes(0) as usize,
                        mib_to_bytes(user_rt_opts.unwrap().memory_limit_mb) as usize,
                    ))
                } else {
                    None
                }
            },
            get_error_class_fn: Some(&get_error_class_name),
            shared_array_buffer_store: None,
            compiled_wasm_module_store: Default::default(),
            startup_snapshot: Some(snapshot::snapshot()),
            ..Default::default()
        });

        // Bootstrapping stage
        let script = format!(
            "globalThis.bootstrapSBEdge({}, {}, {})",
            deno_core::serde_json::json!({ "target": env!("TARGET") }),
            is_user_runtime,
            is_event_worker
        );

        js_runtime
            .execute_script::<String>(located_script_name!(), script)
            .expect("Failed to execute bootstrap script");

        {
            //run inside a closure, so op_state_rc is released
            let env_vars = env_vars.clone();
            let op_state_rc = js_runtime.op_state();
            let mut op_state = op_state_rc.borrow_mut();
            op_state.put::<sb_env::EnvVars>(env_vars);

            if is_event_worker {
                if let WorkerRuntimeOpts::EventsWorker = conf.clone() {
                    // We unwrap because event_manager_opts must always be present when type is `EventsWorker`
                    op_state.put::<mpsc::UnboundedReceiver<WorkerEvents>>(
                        event_manager_opts.unwrap().event_rx,
                    );
                }
            }

            if is_user_runtime {
                if let Some(events_msg_tx) = maybe_events_msg_tx.clone() {
                    op_state.put::<mpsc::UnboundedSender<WorkerEvents>>(events_msg_tx);
                }
            }
        }

        let main_module_id = js_runtime.load_main_module(&main_module_url, None).await?;

        Ok(Self {
            js_runtime,
            main_module_id,
            is_user_runtime,
            env_vars,
            conf,
            is_event_worker,
        })
    }

    pub fn event_sender(&self) -> Option<mpsc::UnboundedSender<WorkerEvents>> {
        if self.is_user_runtime {
            if let WorkerRuntimeOpts::UserWorker(conf) = self.conf.clone() {
                conf.events_msg_tx
            } else {
                None
            }
        } else {
            None
        }
    }

    pub async fn run(
        mut self,
        unix_stream_rx: mpsc::UnboundedReceiver<UnixStream>,
        wall_clock_limit_ms: u64,
    ) -> Result<(), Error> {
        let is_user_rt = self.is_user_runtime;

        {
            let op_state_rc = self.js_runtime.op_state();
            let mut op_state = op_state_rc.borrow_mut();
            op_state.put::<mpsc::UnboundedReceiver<UnixStream>>(unix_stream_rx);

            if !is_user_rt {
                if let WorkerRuntimeOpts::MainWorker(conf) = self.conf.clone() {
                    op_state.put::<mpsc::UnboundedSender<UserWorkerMsgs>>(conf.worker_pool_tx);
                }
            }
        }

        let mut js_runtime = self.js_runtime;

        let future = async move {
            let mod_result_rx = js_runtime.mod_evaluate(self.main_module_id);
            match js_runtime.run_event_loop(false).await {
                Err(err) => {
                    // usually this happens because isolate is terminated
                    debug!("event loop error: {}", err);
                    Err(anyhow!("event loop error: {}", err))
                }
                Ok(_) => match mod_result_rx.await {
                    Err(_) => Err(anyhow!("mod result sender dropped")),
                    Ok(Err(err)) => {
                        debug!("{}", err.to_string());
                        Err(err)
                    }
                    Ok(Ok(_)) => Ok(()),
                },
            }
        };

        // need to set an explicit timeout here in case the event loop idle
        let mut duration = Duration::MAX;
        if is_user_rt {
            duration = Duration::from_millis(wall_clock_limit_ms);
        }
        match tokio::time::timeout(duration, future).await {
            Err(_) => Err(anyhow!("wall clock duration reached")),
            Ok(res) => res,
        }
    }

    #[allow(clippy::wrong_self_convention)]
    // TODO: figure out why rustc complains about this
    #[allow(dead_code)]
    fn to_value<T>(
        &mut self,
        global_value: &deno_core::v8::Global<deno_core::v8::Value>,
    ) -> Result<T, AnyError>
    where
        T: DeserializeOwned + 'static,
    {
        let scope = &mut self.js_runtime.handle_scope();
        let value = deno_core::v8::Local::new(scope, global_value.clone());
        Ok(serde_v8::from_v8(scope, value)?)
    }
}

#[cfg(test)]
mod test {
    use crate::deno_runtime::DenoRuntime;
    use sb_worker_context::essentials::{
        MainWorkerRuntimeOpts, UserWorkerMsgs, UserWorkerRuntimeOpts, WorkerContextInitOpts,
        WorkerRuntimeOpts,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tokio::net::UnixStream;
    use tokio::sync::mpsc;

    async fn create_runtime(
        path: Option<PathBuf>,
        env_vars: Option<HashMap<String, String>>,
        user_conf: Option<WorkerRuntimeOpts>,
    ) -> DenoRuntime {
        let (worker_pool_tx, _) = mpsc::unbounded_channel::<UserWorkerMsgs>();

        DenoRuntime::new(
            WorkerContextInitOpts {
                service_path: path.unwrap_or(PathBuf::from("./test_cases/main")),
                no_module_cache: false,
                import_map_path: None,
                env_vars: env_vars.unwrap_or(Default::default()),
                conf: {
                    if let Some(uc) = user_conf {
                        uc
                    } else {
                        WorkerRuntimeOpts::MainWorker(MainWorkerRuntimeOpts { worker_pool_tx })
                    }
                },
            },
            None,
        )
        .await
        .unwrap()
    }

    // Main Runtime should have access to `EdgeRuntime`
    #[tokio::test]
    async fn test_main_runtime_creation() {
        let mut runtime = create_runtime(None, None, None).await;

        {
            let scope = &mut runtime.js_runtime.handle_scope();
            let context = scope.get_current_context();
            let inner_scope = &mut deno_core::v8::ContextScope::new(scope, context);
            let global = context.global(inner_scope);
            let edge_runtime_key: deno_core::v8::Local<deno_core::v8::Value> =
                deno_core::serde_v8::to_v8(inner_scope, "EdgeRuntime").unwrap();
            assert!(!global
                .get(inner_scope, edge_runtime_key)
                .unwrap()
                .is_undefined(),);
        }
    }

    // User Runtime Should not have access to EdgeRuntime
    #[tokio::test]
    async fn test_user_runtime_creation() {
        let mut runtime = create_runtime(
            None,
            None,
            Some(WorkerRuntimeOpts::UserWorker(Default::default())),
        )
        .await;

        {
            let scope = &mut runtime.js_runtime.handle_scope();
            let context = scope.get_current_context();
            let inner_scope = &mut deno_core::v8::ContextScope::new(scope, context);
            let global = context.global(inner_scope);
            let edge_runtime_key: deno_core::v8::Local<deno_core::v8::Value> =
                deno_core::serde_v8::to_v8(inner_scope, "EdgeRuntime").unwrap();
            assert!(global
                .get(inner_scope, edge_runtime_key)
                .unwrap()
                .is_undefined(),);
        }
    }

    #[tokio::test]
    async fn test_main_rt_fs() {
        let mut main_rt = create_runtime(None, Some(std::env::vars().collect()), None).await;

        let global_value_deno_read_file_script = main_rt
            .js_runtime
            .execute_script(
                "<anon>",
                r#"
            Deno.readTextFileSync("./test_cases/readFile/hello_world.json");
        "#,
            )
            .unwrap();
        let fs_read_result =
            main_rt.to_value::<deno_core::serde_json::Value>(&global_value_deno_read_file_script);
        assert_eq!(
            fs_read_result.unwrap().as_str().unwrap(),
            "{\n  \"hello\": \"world\"\n}"
        );
    }

    #[tokio::test]
    async fn test_os_ops() {
        let mut user_rt = create_runtime(
            None,
            None,
            Some(WorkerRuntimeOpts::UserWorker(Default::default())),
        )
        .await;

        let user_rt_execute_scripts = user_rt
            .js_runtime
            .execute_script(
                "<anon>",
                r#"
            // Should not be able to set
            const data = {
                gid: Deno.gid(),
                uid: Deno.uid(),
                hostname: Deno.hostname(),
                loadavg: Deno.loadavg(),
                osUptime: Deno.osUptime(),
                osRelease: Deno.osRelease(),
                systemMemoryInfo: Deno.systemMemoryInfo(),
                consoleSize: Deno.consoleSize(),
                version: [Deno.version.deno, Deno.version.v8, Deno.version.typescript],
                networkInterfaces: Deno.networkInterfaces()
            };
            data;
        "#,
            )
            .unwrap();
        let serde_deno_env = user_rt
            .to_value::<deno_core::serde_json::Value>(&user_rt_execute_scripts)
            .unwrap();
        assert_eq!(serde_deno_env.get("gid").unwrap().as_i64().unwrap(), 1000);
        assert_eq!(serde_deno_env.get("uid").unwrap().as_i64().unwrap(), 1000);
        assert!(serde_deno_env.get("osUptime").unwrap().as_i64().unwrap() > 0);
        assert_eq!(
            serde_deno_env.get("osRelease").unwrap().as_str().unwrap(),
            "0.0.0-00000000-generic"
        );

        let loadavg_array = serde_deno_env
            .get("loadavg")
            .unwrap()
            .as_array()
            .unwrap()
            .to_vec();
        assert_eq!(loadavg_array.get(0).unwrap().as_f64().unwrap(), 0.0);
        assert_eq!(loadavg_array.get(1).unwrap().as_f64().unwrap(), 0.0);
        assert_eq!(loadavg_array.get(2).unwrap().as_f64().unwrap(), 0.0);

        let network_interfaces_data = serde_deno_env
            .get("networkInterfaces")
            .unwrap()
            .as_array()
            .unwrap()
            .to_vec();
        assert_eq!(network_interfaces_data.len(), 2);

        let deno_version_array = serde_deno_env
            .get("version")
            .unwrap()
            .as_array()
            .unwrap()
            .to_vec();
        assert_eq!(deno_version_array.get(0).unwrap().as_str().unwrap(), "");
        assert_eq!(deno_version_array.get(1).unwrap().as_str().unwrap(), "");
        assert_eq!(deno_version_array.get(2).unwrap().as_str().unwrap(), "");

        let system_memory_info_map = serde_deno_env
            .get("systemMemoryInfo")
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        assert!(system_memory_info_map.contains_key("total"));
        assert!(system_memory_info_map.contains_key("free"));
        assert!(system_memory_info_map.contains_key("available"));
        assert!(system_memory_info_map.contains_key("buffers"));
        assert!(system_memory_info_map.contains_key("cached"));
        assert!(system_memory_info_map.contains_key("swapTotal"));
        assert!(system_memory_info_map.contains_key("swapFree"));

        let deno_consle_size_map = serde_deno_env
            .get("consoleSize")
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        assert!(deno_consle_size_map.contains_key("rows"));
        assert!(deno_consle_size_map.contains_key("columns"));

        let user_rt_execute_scripts = user_rt.js_runtime.execute_script(
            "<anon>",
            r#"
            let cmd = new Deno.Command("", {});
            cmd.outputSync();
        "#,
        );
        assert!(user_rt_execute_scripts.is_err());
        assert!(user_rt_execute_scripts
            .unwrap_err()
            .to_string()
            .contains("Spawning subprocesses is not allowed on Supabase Edge Runtime"));
    }

    #[tokio::test]
    async fn test_os_env_vars() {
        std::env::set_var("Supa_Test", "Supa_Value");
        let mut main_rt = create_runtime(None, Some(std::env::vars().collect()), None).await;
        let mut user_rt = create_runtime(
            None,
            None,
            Some(WorkerRuntimeOpts::UserWorker(Default::default())),
        )
        .await;
        assert!(!main_rt.env_vars.is_empty());
        assert!(user_rt.env_vars.is_empty());

        let err = main_rt
            .js_runtime
            .execute_script(
                "<anon>",
                r#"
            // Should not be able to set
            Deno.env.set("Supa_Test", "Supa_Value");
        "#,
            )
            .err()
            .unwrap();
        assert!(err
            .to_string()
            .contains("NotSupported: The operation is not supported"));

        let main_deno_env_get_supa_test = main_rt
            .js_runtime
            .execute_script(
                "<anon>",
                r#"
            // Should not be able to set
            Deno.env.get("Supa_Test");
        "#,
            )
            .unwrap();
        let serde_deno_env =
            main_rt.to_value::<deno_core::serde_json::Value>(&main_deno_env_get_supa_test);
        assert_eq!(serde_deno_env.unwrap().as_str().unwrap(), "Supa_Value");

        // User does not have this env variable because it was not provided
        // During the runtime creation
        let user_deno_env_get_supa_test = user_rt
            .js_runtime
            .execute_script(
                "<anon>",
                r#"
            // Should not be able to set
            Deno.env.get("Supa_Test");
        "#,
            )
            .unwrap();
        let user_serde_deno_env =
            user_rt.to_value::<deno_core::serde_json::Value>(&user_deno_env_get_supa_test);
        assert!(user_serde_deno_env.unwrap().is_null());
    }

    async fn create_basic_user_runtime(
        path: &str,
        memory_limit: u64,
        worker_timeout_ms: u64,
    ) -> DenoRuntime {
        create_runtime(
            Some(PathBuf::from(path)),
            None,
            Some(WorkerRuntimeOpts::UserWorker(UserWorkerRuntimeOpts {
                memory_limit_mb: memory_limit,
                worker_timeout_ms,
                force_create: true,
                key: None,
                pool_msg_tx: None,
                events_msg_tx: None,
            })),
        )
        .await
    }

    #[tokio::test]
    async fn test_read_file_user_rt() {
        let user_rt = create_basic_user_runtime("./test_cases/readFile", 5, 1000).await;
        let (_tx, unix_stream_rx) = mpsc::unbounded_channel::<UnixStream>();
        let wall_clock_limit_ms = 100 * 60 * 1000;
        let result = user_rt.run(unix_stream_rx, wall_clock_limit_ms).await;
        match result {
            Err(err) => {
                assert!(err
                    .to_string()
                    .contains("TypeError: Deno.readFileSync is not a function"));
            }
            _ => panic!("Invalid Result"),
        };
    }
}
