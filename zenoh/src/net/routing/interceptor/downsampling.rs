//
// Copyright (c) 2024 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

//! ⚠️ WARNING ⚠️
//!
//! This module is intended for Zenoh's internal use.
//!
//! [Click here for Zenoh's documentation](https://docs.rs/zenoh/latest/zenoh)

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tracing::trace;
use zenoh_config::{DownsamplingItemConf, DownsamplingRuleConf, InterceptorFlow};
use zenoh_core::zlock;
use zenoh_keyexpr::keyexpr_tree::{
    impls::KeyedSetProvider, support::UnknownWildness, IKeyExprTree, IKeyExprTreeMut, KeBoxTree,
};
use zenoh_protocol::network::NetworkBody;
use zenoh_result::ZResult;
use tokio::sync::broadcast;
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::task::JoinHandle;
use crate::net::routing::interceptor::*;

lazy_static! {
    pub static ref DSM: Arc<(Sender<DownsamplingItemConf>, Receiver<DownsamplingItemConf>)> = Arc::new(broadcast::channel(256));
}

pub(crate) fn downsampling_interceptor_factories(
    config: &Vec<DownsamplingItemConf>,
) -> ZResult<Vec<InterceptorFactory>> {
    let mut res: Vec<InterceptorFactory> = vec![];

    let mut id_set = HashSet::new();
    for ds in config {
        // check unicity of rule id
        if let Some(id) = &ds.id {
            if !id_set.insert(id.clone()) {
                bail!("Invalid Downsampling config: id '{id}' is repeated");
            }
        }
        res.push(Box::new(DownsamplingInterceptorFactory::new(ds.clone())));
    }

    Ok(res)
}

pub struct DownsamplingInterceptorFactory {
    interfaces: Option<Vec<String>>,
    rules: Vec<DownsamplingRuleConf>,
    flow: InterceptorFlow,
}

impl DownsamplingInterceptorFactory {
    pub fn new(conf: DownsamplingItemConf) -> Self {
        Self {
            interfaces: conf.interfaces,
            rules: conf.rules,
            flow: conf.flow,
        }
    }
}

impl InterceptorFactoryTrait for DownsamplingInterceptorFactory {
    fn new_transport_unicast(
        &self,
        transport: &TransportUnicast,
    ) -> (Option<IngressInterceptor>, Option<EgressInterceptor>) {
        tracing::debug!("New downsampler transport unicast {:?}", transport);
        if let Some(interfaces) = &self.interfaces {
            tracing::debug!(
                "New downsampler transport unicast config interfaces: {:?}",
                interfaces
            );
            if let Ok(links) = transport.get_links() {
                for link in links {
                    tracing::debug!(
                        "New downsampler transport unicast link interfaces: {:?}",
                        link.interfaces
                    );
                    if !link.interfaces.iter().any(|x| interfaces.contains(x)) {
                        return (None, None);
                    }
                }
            }
        };
        match self.flow {
            InterceptorFlow::Ingress => (
                Some(Box::new(ComputeOnMiss::new(DownsamplingInterceptor::new(
                    self.rules.clone(),
                )))),
                None,
            ),
            InterceptorFlow::Egress => (
                None,
                Some(Box::new(ComputeOnMiss::new(DownsamplingInterceptor::new(
                    self.rules.clone(),
                )))),
            ),
        }
    }

    fn new_transport_multicast(
        &self,
        _transport: &TransportMulticast,
    ) -> Option<EgressInterceptor> {
        None
    }

    fn new_peer_multicast(&self, _transport: &TransportMulticast) -> Option<IngressInterceptor> {
        None
    }
}

pub struct Timestate {
    threshold: tokio::time::Duration,
    latest_message_timestamp: tokio::time::Instant,
}

pub(crate) struct DownsamplingInterceptor {
    ke_id: Arc<Mutex<KeBoxTree<usize, UnknownWildness, KeyedSetProvider>>>,
    ke_state: Arc<Mutex<HashMap<usize, Timestate>>>,
    handle: Option<JoinHandle<()>>
}

impl Drop for DownsamplingInterceptor {
    fn drop(&mut self) {
        tracing::debug!("DownSampling Interceptor dropped. Removeing task.");
        self.handle.take().unwrap().abort();
    }
}

impl InterceptorTrait for DownsamplingInterceptor {
    fn compute_keyexpr_cache(&self, key_expr: &KeyExpr<'_>) -> Option<Box<dyn Any + Send + Sync>> {
        let ke_id = zlock!(self.ke_id);
        if let Some(node) = ke_id.intersecting_keys(key_expr).next() {
            if let Some(id) = ke_id.weight_at(&node) {
                return Some(Box::new(Some(*id)));
            }
        }
        Some(Box::new(None::<usize>))
    }

    fn intercept(
        &self,
        ctx: RoutingContext<NetworkMessage>,
        cache: Option<&Box<dyn Any + Send + Sync>>,
    ) -> Option<RoutingContext<NetworkMessage>> {
        if matches!(ctx.msg.body, NetworkBody::Push(_)) {
            if let Some(cache) = cache {
                if let Some(id) = cache.downcast_ref::<Option<usize>>() {
                    if let Some(id) = id {
                        let mut ke_state = zlock!(self.ke_state);
                        if let Some(state) = ke_state.get_mut(id) {
                            let timestamp = tokio::time::Instant::now();

                            if timestamp - state.latest_message_timestamp >= state.threshold {
                                state.latest_message_timestamp = timestamp;
                                return Some(ctx);
                            } else {
                                return None;
                            }
                        } else {
                            tracing::debug!("unexpected cache ID {}", id);
                        }
                    }
                } else {
                    tracing::debug!("unexpected cache type {:?}", ctx.full_expr());
                }
            }
        }

        Some(ctx)
    }
}

const NANOS_PER_SEC: f64 = 1_000_000_000.0;

impl DownsamplingInterceptor {
    pub fn new(rules: Vec<DownsamplingRuleConf>) -> Self {
        let ke_id = Arc::new(Mutex::new(KeBoxTree::default()));
        let ke_state = Arc::new(Mutex::new(HashMap::default()));
        for (id, rule) in rules.into_iter().enumerate() {
           Self::update_downsampling_rule(&ke_id, &ke_state, id, &rule);
        }

        let dsm = DSM.clone();
        let ke_i = ke_id.clone();
        let ke_s = ke_state.clone();
        let handle = Some(tokio::spawn(async move {
            loop {
                let conf = dsm.0.subscribe().recv().await.unwrap();
                tracing::debug!("Trying to update DownSamplerInteceptor with new config ...");
                let rules = conf.rules;
                for (id, rule) in rules.into_iter().enumerate() {
                    Self::update_downsampling_rule(&ke_i, &ke_s, id, &rule);
                }
            }
        }));

        Self {
            ke_id,
            ke_state,
            handle
        }
    }

    fn update_downsampling_rule(ke_id: &Arc<Mutex<KeBoxTree<usize, UnknownWildness>>>, ke_state: &Arc<Mutex<HashMap<usize, Timestate>>>, id: usize, rule: &DownsamplingRuleConf) {
        let mut threshold = tokio::time::Duration::MAX;
        let mut latest_message_timestamp = tokio::time::Instant::now();
        if rule.freq != 0.0 {
            threshold =
                tokio::time::Duration::from_nanos((1. / rule.freq * NANOS_PER_SEC) as u64);
            latest_message_timestamp -= threshold;
        }
        ke_id.lock().unwrap().insert(&rule.key_expr, id);
        ke_state.lock().unwrap().insert(
            id,
            Timestate {
                threshold,
                latest_message_timestamp,
            },
        );
        tracing::debug!(
                "New downsampler rule enabled: key_expr={:?}, threshold={:?}",
                rule.key_expr,
                threshold
            );
    }
}
