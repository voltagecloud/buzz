//! A minimal in-memory apiserver for one namespace, driving the real
//! `kube::Client` request path (`tower::service_fn`, as `cluster.rs` tests do).
//!
//! It models only what the Sandbox paths touch: Sandbox CRUD with
//! resourceVersion CAS, a controller that creates the Sandbox's Pod while it
//! is Running, Pods whose agent container runs once their `envFrom` Secret
//! exists (or whose status a test overrides), and Secret create with failure
//! injection.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use http::{Method, Request, Response};
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::ByteString;
use kube::client::Body;
use kube::Client;
use serde_json::{json, Value};

#[derive(Default)]
pub struct State {
    pub sandboxes: BTreeMap<String, Value>,
    pub secrets: BTreeMap<String, Value>,
    /// Pod UIDs by name; Pods are rendered from their Sandbox on read.
    pub pods: BTreeMap<String, String>,
    /// Pod `creationTimestamp`s (RFC 3339) by name; set when the controller
    /// creates the Pod.
    pub pod_created: BTreeMap<String, String>,
    /// Fields that replace the derived agent container status, such as
    /// `restartCount` or `lastState`.
    pub container_status: Option<Value>,
    pub rv: u64,
    pub uid: u64,
    /// Fail this many Secret creates with a 500.
    pub fail_secret_creates: usize,
    /// Store the Secret, then fail this many create responses with a 500.
    pub fail_secret_create_responses: usize,
    /// Answer this many Sandbox patches with a 409 Conflict.
    pub conflict_patches: usize,
    /// Every merge patch body received.
    pub patches: Vec<Value>,
}

pub type Shared = Arc<Mutex<State>>;

fn status(code: u16, reason: &str) -> Response<Body> {
    let body = json!({"kind":"Status","apiVersion":"v1","status":"Failure",
        "message":reason,"reason":reason,"code":code});
    Response::builder()
        .status(code)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn ok(value: &Value) -> Response<Body> {
    Response::builder()
        .status(200)
        .body(Body::from(serde_json::to_vec(value).unwrap()))
        .unwrap()
}

fn merge(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    target.remove(key);
                } else {
                    merge(target.entry(key.clone()).or_insert(Value::Null), value);
                }
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

impl State {
    fn next_rv(&mut self) -> String {
        self.rv += 1;
        self.rv.to_string()
    }

    fn next_uid(&mut self, kind: &str) -> String {
        self.uid += 1;
        format!("{kind}-uid-{}", self.uid)
    }

    /// The controller: a Running Sandbox has a Pod; Suspended has none.
    fn reconcile(&mut self, name: &str) {
        let running = self
            .sandboxes
            .get(name)
            .is_some_and(|s| s["spec"]["operatingMode"] == "Running");
        if running && !self.pods.contains_key(name) {
            let uid = self.next_uid("pod");
            self.pods.insert(name.into(), uid);
            self.pod_created
                .insert(name.into(), chrono::Utc::now().to_rfc3339());
        } else if !running {
            self.pods.remove(name);
            self.pod_created.remove(name);
        }
    }

    fn pod(&self, name: &str) -> Option<Value> {
        let uid = self.pods.get(name)?;
        let sandbox = self.sandboxes.get(name)?;
        let template = &sandbox["spec"]["podTemplate"];
        let secret = template["spec"]["containers"][0]["envFrom"][0]["secretRef"]["name"]
            .as_str()
            .unwrap_or_default();
        // `envFrom` names the generation's Secret; the container starts once
        // it exists.
        let started = self.secrets.contains_key(secret);
        let state = if started {
            json!({"running": {}})
        } else {
            json!({"waiting": {"reason": "CreateContainerConfigError"}})
        };
        let mut container = json!({
            "name": "agent", "image": "i", "imageID": "", "ready": started,
            "restartCount": 0, "state": state});
        if let Some(Value::Object(fields)) = &self.container_status {
            for (key, value) in fields {
                container[key] = value.clone();
            }
        }
        Some(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {
                "name": name, "uid": uid,
                "creationTimestamp": self.pod_created.get(name),
                "labels": template["metadata"]["labels"],
                "annotations": template["metadata"]["annotations"],
                "ownerReferences": [{"apiVersion": "agents.x-k8s.io/v1beta1",
                    "kind": "Sandbox", "name": name,
                    "uid": sandbox["metadata"]["uid"], "controller": true}],
            },
            "status": {"phase": "Pending", "containerStatuses": [container]},
        }))
    }

    fn handle(&mut self, method: &Method, path: &str, body: &[u8]) -> Response<Body> {
        let path = path.split('?').next().unwrap_or_default();
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match segments.as_slice() {
            ["apis", "agents.x-k8s.io", "v1beta1", "namespaces", _, "sandboxes", rest @ ..] => {
                self.sandbox(method, rest.first().copied(), body)
            }
            ["api", "v1", "namespaces", _, "pods"] => {
                let names: Vec<String> = self.pods.keys().cloned().collect();
                let items: Vec<Value> = names.iter().filter_map(|n| self.pod(n)).collect();
                ok(&json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":items}))
            }
            ["api", "v1", "namespaces", _, "pods", name] => match self.pod(name) {
                Some(pod) => ok(&pod),
                None => status(404, "NotFound"),
            },
            ["api", "v1", "namespaces", _, "secrets", name] => match self.secrets.get(*name) {
                Some(secret) => ok(secret),
                None => status(404, "NotFound"),
            },
            ["api", "v1", "namespaces", _, "secrets"] if method == Method::POST => {
                if self.fail_secret_creates > 0 {
                    self.fail_secret_creates -= 1;
                    return status(500, "InternalError");
                }
                let mut secret: Secret = serde_json::from_slice(body).unwrap();
                let name = secret.metadata.name.clone().unwrap();
                if self.secrets.contains_key(&name) {
                    return status(409, "AlreadyExists");
                }
                secret.data = secret.string_data.take().map(|data| {
                    data.into_iter()
                        .map(|(k, v)| (k, ByteString(v.into_bytes())))
                        .collect()
                });
                secret.metadata.resource_version = Some(self.next_rv());
                secret.metadata.uid = Some(self.next_uid("secret"));
                let stored = serde_json::to_value(&secret).unwrap();
                self.secrets.insert(name, stored.clone());
                if self.fail_secret_create_responses > 0 {
                    self.fail_secret_create_responses -= 1;
                    return status(500, "InternalError");
                }
                ok(&stored)
            }
            _ => status(404, "NotFound"),
        }
    }

    fn sandbox(&mut self, method: &Method, name: Option<&str>, body: &[u8]) -> Response<Body> {
        match (method.clone(), name) {
            (Method::GET, Some(name)) => match self.sandboxes.get(name) {
                Some(sandbox) => ok(sandbox),
                None => status(404, "NotFound"),
            },
            (Method::POST, None) => {
                let mut sandbox: Value = serde_json::from_slice(body).unwrap();
                let name = sandbox["metadata"]["name"].as_str().unwrap().to_string();
                if self.sandboxes.contains_key(&name) {
                    return status(409, "AlreadyExists");
                }
                sandbox["metadata"]["uid"] = self.next_uid("sandbox").into();
                sandbox["metadata"]["resourceVersion"] = self.next_rv().into();
                self.sandboxes.insert(name.clone(), sandbox.clone());
                self.reconcile(&name);
                ok(&sandbox)
            }
            (Method::PUT, Some(name)) => {
                let mut sandbox: Value = serde_json::from_slice(body).unwrap();
                let Some(current) = self.sandboxes.get(name) else {
                    return status(404, "NotFound");
                };
                if current["metadata"]["resourceVersion"] != sandbox["metadata"]["resourceVersion"]
                {
                    return status(409, "Conflict");
                }
                sandbox["metadata"]["resourceVersion"] = self.next_rv().into();
                self.sandboxes.insert(name.into(), sandbox.clone());
                self.reconcile(name);
                ok(&sandbox)
            }
            (Method::PATCH, Some(name)) => {
                let patch: Value = serde_json::from_slice(body).unwrap();
                self.patches.push(patch.clone());
                if self.conflict_patches > 0 {
                    self.conflict_patches -= 1;
                    return status(409, "Conflict");
                }
                let rv = self.next_rv();
                let Some(current) = self.sandboxes.get_mut(name) else {
                    return status(404, "NotFound");
                };
                let expected = &patch["metadata"]["resourceVersion"];
                if !expected.is_null() && *expected != current["metadata"]["resourceVersion"] {
                    return status(409, "Conflict");
                }
                merge(current, &patch);
                current["metadata"]["resourceVersion"] = rv.into();
                let updated = current.clone();
                self.reconcile(name);
                ok(&updated)
            }
            _ => status(405, "MethodNotAllowed"),
        }
    }
}

/// A client backed by `state`.
pub fn client(state: &Shared) -> Client {
    let state = Arc::clone(state);
    let service = tower::service_fn(move |request: Request<Body>| {
        let state = Arc::clone(&state);
        async move {
            let (parts, body) = request.into_parts();
            let bytes = http_body_util::BodyExt::collect(body)
                .await
                .map(|b| b.to_bytes().to_vec())
                .unwrap_or_default();
            let response =
                state
                    .lock()
                    .unwrap()
                    .handle(&parts.method, &parts.uri.to_string(), &bytes);
            Ok::<_, std::convert::Infallible>(response)
        }
    });
    Client::new(service, "agents")
}
