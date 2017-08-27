//! Pre/post cache/request hooks

use client_query::ClientQuery;
use dnssector::{DNSSector, ParsedPacket};
use dnssector::c_abi::{self, FnTable};
use glob::glob;
use libloading::{self, Library};
#[cfg(unix)]
use libloading::os::unix::Symbol;
#[cfg(windows)]
use libloading::os::windows::Symbol;
use nix::libc::{c_int, c_void};
use qp_trie::Trie;
use std::ffi::OsStr;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;

const MASTER_SERVICE_LIBRARY_NAME: &'static str = "master";
const DLL_EXT: &'static str = "dylib";

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionState;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Pass = 1,
    Lookup,
    Drop,
}

impl From<Action> for c_int {
    fn from(v: Action) -> c_int {
        v as c_int
    }
}

impl From<c_int> for Action {
    fn from(id: c_int) -> Action {
        match id {
            x if x == Action::Pass.into() => Action::Pass,
            x if x == Action::Lookup.into() => Action::Lookup,
            _ => Action::Drop,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Stage {
    Recv,
    Deliver,
}

type HookSymbolClientT = unsafe extern "C" fn(*const FnTable, *mut ParsedPacket) -> c_int;

struct ServiceHooks {
    library: Arc<Library>,
    hook_recv: Option<Symbol<HookSymbolClientT>>,
    hook_deliver: Option<Symbol<HookSymbolClientT>>,
}

struct Service {
    service_hooks: Option<ServiceHooks>,
}

pub struct Hooks {
    services: Trie<Vec<u8>, Service>,
    master_service_id: Vec<u8>,
    libraries_path: Option<String>,
}

impl Service {
    fn new(library_path: Option<&str>) -> Result<Service, &'static str> {
        let library_path = match library_path {
            None => {
                return Ok(Service {
                    service_hooks: None,
                })
            }
            Some(library_path) => library_path,
        };
        let library = match Library::new(library_path) {
            Err(e) => {
                error!("Cannot load the dynamic library [{}] [{}]", library_path, e);
                return Err("Unable to load the dynamic library");
            }
            Ok(library) => Arc::new(library),
        };

        let library_inner = library.clone();

        let hook_recv_hl: libloading::Result<libloading::Symbol<HookSymbolClientT>> =
            unsafe { library_inner.get(b"hook_recv") };
        let hook_recv = hook_recv_hl.ok().map(|hook| unsafe { hook.into_raw() });

        let hook_deliver_hl: libloading::Result<libloading::Symbol<HookSymbolClientT>> =
            unsafe { library_inner.get(b"hook_deliver") };
        let hook_deliver = hook_deliver_hl.ok().map(|hook| unsafe { hook.into_raw() });

        let service_hooks = ServiceHooks {
            library,
            hook_recv,
            hook_deliver,
        };
        Ok(Service {
            service_hooks: Some(service_hooks),
        })
    }
}

impl Hooks {
    fn load_library(&mut self, library_path: &PathBuf) -> Result<(), &'static str> {
        let stem = match library_path.file_stem() {
            None => return Err("Missing stem from file name"),
            Some(stem) => stem,
        };
        debug!("Loading dynamic library [{}]", library_path.display());
        let services = &mut self.services;
        let service_id = if stem == MASTER_SERVICE_LIBRARY_NAME {
            info!("Loading master dynamic library");
            &self.master_service_id
        } else {
            match stem.to_str() {
                None => return Err("Unsupported path name"),
                Some(stem) => stem.as_bytes(),
            }
        };
        if stem == MASTER_SERVICE_LIBRARY_NAME {
            let library_path_str = match library_path.to_str() {
                None => return Err("Unsupported path name"),
                Some(path_str) => path_str,
            };
            let service = match Service::new(Some(library_path_str)) {
                Ok(service) => service,
                Err(_) => return Err("Unable to register the service"),
            };
            services.insert(service_id.to_vec(), service);
        }
        Ok(())
    }

    fn load_libraries(&mut self) {
        let path_expr = {
            let libraries_path = match self.libraries_path {
                None => return,
                Some(ref libraries_path) => libraries_path,
            };
            format!("{}/*.{}", libraries_path, DLL_EXT)
        };
        for library_path in glob(&path_expr).expect("Unsupported path for dynamic libraries") {
            let library_path = match library_path {
                Err(_) => continue,
                Ok(ref library_path) => library_path,
            };
            match self.load_library(&library_path) {
                Ok(()) => {}
                Err(e) => warn!("[{}]: {}", library_path.display(), e),
            }
        }
    }

    pub fn new(libraries_path: Option<&str>) -> Self {
        let services = Trie::new();
        let master_service_id = Vec::new();
        let mut hooks = Hooks {
            services,
            master_service_id,
            libraries_path: libraries_path.map(|x| x.to_owned()),
        };
        hooks.load_libraries();
        hooks
    }

    #[inline]
    pub fn enabled(&self, _stage: Stage) -> bool {
        let service = self.services.get(&self.master_service_id);
        service
            .expect("Nonexistent service")
            .service_hooks
            .is_some()
    }

    pub fn apply_clientside(
        &self,
        session_state: SessionState,
        packet: Vec<u8>,
        stage: Stage,
    ) -> Result<(Action, Vec<u8>), &'static str> {
        if !self.enabled(stage) {
            return Ok((Action::Pass, packet));
        }
        let ds = match DNSSector::new(packet) {
            Ok(ds) => ds,
            Err(e) => {
                warn!("Cannot parse packet: {}", e);
                return Err("Cannot parse packet");
            }
        };
        let mut parsed_packet = match ds.parse() {
            Ok(parsed_packet) => parsed_packet,
            Err(e) => {
                warn!("Invalid packet: {}", e);
                return Err("Invalid packet");
            }
        };
        let service = self.services
            .get(&self.master_service_id)
            .expect("Nonexistent master service");
        let service_hooks = service.service_hooks.as_ref().unwrap();
        let hook = match stage {
            Stage::Recv => service_hooks.hook_recv.as_ref().unwrap(),
            Stage::Deliver => service_hooks.hook_deliver.as_ref().unwrap(),
        };
        let fn_table = c_abi::fn_table();
        let action = unsafe { hook(&fn_table, &mut parsed_packet) }.into();

        let packet = parsed_packet.into_packet();
        Ok((action, packet))
    }

    pub fn apply_serverside(
        &self,
        packet: Vec<u8>,
        stage: Stage,
    ) -> Result<(Action, Vec<u8>), &'static str> {
        if !self.enabled(stage) {
            return Ok((Action::Pass, packet));
        }
        let ds = match DNSSector::new(packet) {
            Ok(ds) => ds,
            Err(e) => {
                warn!("Cannot parse packet: {}", e);
                return Err("Cannot parse packet");
            }
        };
        let mut parsed_packet = match ds.parse() {
            Ok(parsed_packet) => parsed_packet,
            Err(e) => {
                warn!("Invalid packet: {}", e);
                return Err("Invalid packet");
            }
        };
        let service = self.services
            .get(&self.master_service_id)
            .expect("Nonexistent master service");
        let service_hooks = service.service_hooks.as_ref().unwrap();
        let hook = match stage {
            Stage::Recv => service_hooks.hook_recv.as_ref().unwrap(),
            Stage::Deliver => service_hooks.hook_deliver.as_ref().unwrap(),
        };
        let fn_table = c_abi::fn_table();
        let action = unsafe { hook(&fn_table, &mut parsed_packet) }.into();
        let packet = parsed_packet.into_packet();
        Ok((action, packet))
    }
}
