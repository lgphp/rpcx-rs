#![allow(non_snake_case)]

use std::collections::HashMap;

use super::selector::ClientSelector;

use super::client::{Client, Opt};
use super::RpcxClient;
use futures::future;
use futures::Future;
use rpcx_protocol::{Error, Metadata, Result, RpcxParam};
use std::boxed::Box;
use std::cell::RefCell;
use std::sync::{Arc, RwLock, RwLockWriteGuard};
use strum_macros::{Display, EnumIter, EnumString};

pub trait ServiceDiscovery {
    fn get_services() -> [(String, String)];
    fn close();
}

#[derive(Debug, Copy, Clone, Display, PartialEq, EnumIter, EnumString)]
pub enum FailMode {
    //Failover selects another server automaticaly
    Failover = 0,
    //Failfast returns error immediately
    Failfast = 1,
    //Failtry use current client again
    Failtry = 2,
    //Failbackup select another server if the first server doesn't respon in specified time and use the fast response.
    Failbackup = 3,
}

#[derive(Debug, Copy, Clone, Display, PartialEq, EnumIter, EnumString)]
pub enum SelectMode {
    //RandomSelect is selecting randomly
    RandomSelect = 0,
    //RoundRobin is selecting by round robin
    RoundRobin = 1,
    //WeightedRoundRobin is selecting by weighted round robin
    WeightedRoundRobin = 2,
    //WeightedICMP is selecting by weighted Ping time
    WeightedICMP = 3,
    //ConsistentHash is selecting by hashing
    ConsistentHash = 4,
    //Closest is selecting the closest server
    Closest = 5,
    // SelectByUser is selecting by implementation of users
    SelectByUser = 1000,
}

pub struct XClient<S: ClientSelector> {
    pub opt: Opt,
    fail_mode: FailMode,
    clients: Arc<RwLock<HashMap<String, RefCell<Client>>>>,
    selector: S,
}

impl<S: ClientSelector> XClient<S> {
    pub fn new(fm: FailMode, s: S, opt: Opt) -> Self {
        XClient {
            fail_mode: fm,
            selector: s,
            clients: Arc::new(RwLock::new(HashMap::new())),
            opt: opt,
        }
    }

    fn get_cached_client<'a>(
        &'a self,
        clients_guard: &'a mut RwLockWriteGuard<HashMap<String, RefCell<Client>>>,
        k: String,
    ) -> Result<&'a mut RefCell<Client>> {
        let client = clients_guard.get_mut(&k);
        if client.is_none() {
            drop(client);
            match clients_guard.get(&k) {
                Some(_) => {}
                None => {
                    let mut items: Vec<&str> = k.split("@").collect();
                    if items.len() == 1 {
                        items.insert(0, "tcp");
                    }
                    let mut created_client = Client::new(&items[1]);
                    created_client.opt = self.opt;
                    match created_client.start() {
                        Ok(_) => {
                            clients_guard.insert(k.clone(), RefCell::new(created_client));
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
        }

        let client = clients_guard.get_mut(&k);
        match client {
            Some(_) => Ok(client.unwrap()),
            None => Err(Error::from("client still not found".to_owned())),
        }
    }
}

impl<S: ClientSelector> RpcxClient for XClient<S> {
    fn call<T>(
        &mut self,
        service_path: &String,
        service_method: &String,
        is_oneway: bool,
        metadata: &Metadata,
        args: &dyn RpcxParam,
    ) -> Option<Result<T>>
    where
        T: RpcxParam + Default,
    {
        // get a key from selector
        let selector = &mut (self.selector);
        let k = selector.select(&service_path, &service_method, args);
        if k.is_empty() {
            return Some(Err(Error::from("server not found".to_owned())));
        }

        let mut clients_guard = self.clients.write().unwrap();
        let client = self.get_cached_client(&mut clients_guard, k.clone());
        if client.is_err() {
            return Some(Err(client.unwrap_err()));
        }
        // invoke this client
        let mut selected_client = client.unwrap().borrow_mut();
        let opt_rt =
            (*selected_client).call::<T>(service_path, service_method, is_oneway, metadata, args);

        if is_oneway {
            return opt_rt;
        }

        let rt = opt_rt.unwrap();

        if rt.is_err() {
            match self.fail_mode {
                FailMode::Failover => {}
                FailMode::Failfast => return Some(rt),
                FailMode::Failtry => {
                    let mut retry = self.opt.retry;
                    while retry > 0 {
                        retry -= 1;
                        let opt_rt = (*selected_client).call::<T>(
                            service_path,
                            service_method,
                            is_oneway,
                            metadata,
                            args,
                        );
                        let rt = opt_rt.unwrap();
                        if rt.is_ok() {
                            return Some(rt);
                        }
                    }
                }
                FailMode::Failbackup => {}
            }
        }

        Some(rt)
    }
    fn acall<T>(
        &mut self,
        service_path: &String,
        service_method: &String,
        metadata: &Metadata,
        args: &dyn RpcxParam,
    ) -> Box<dyn Future<Item = Result<T>, Error = Error> + Send + Sync>
    where
        T: RpcxParam + Default + Sync + Send + 'static,
    {
        // get a key from selector
        let k = self.selector.select(&service_path, &service_method, args);
        if k.is_empty() {
            return Box::new(future::err(Error::from("server not found".to_owned())));
        }

        let mut clients_guard = self.clients.write().unwrap();
        let client = self.get_cached_client(&mut clients_guard, k.clone());

        if client.is_err() {
            return Box::new(future::err(client.unwrap_err()));
        }

        // invoke this client
        let mut selected_client = client.unwrap().borrow_mut();
        let rt = (*selected_client).acall::<T>(service_path, service_method, metadata, args);
        rt
    }
}