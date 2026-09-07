//! Workspace-wide policy independent of HTTP, randomness, clocks and async I/O.
//! Host supplies private capabilities, public IDs and monotonic elapsed seconds.
use notebook_protocol::{ErrorCode, NotebookSnapshot, ProtocolError};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
type Result<T> = std::result::Result<T, ProtocolError>;
fn fail(code: ErrorCode, message: &str) -> ProtocolError {
    ProtocolError {
        code,
        message: message.into(),
        retryable: false,
    }
}
#[derive(Default)]
pub struct Room {
    pub members: BTreeSet<String>,
    pub sequence: u64,
    pub snapshot: Option<NotebookSnapshot>,
    pub origin: Option<String>,
    pub active: usize,
}
struct Member {
    token: String,
    id: String,
    touched: u64,
}
pub type DriverPolicy = fn(&[String]) -> Option<String>;
pub fn first_connected(ids: &[String]) -> Option<String> {
    ids.first().cloned()
}
pub struct Collaboration {
    pub rooms: BTreeMap<String, Room>,
    members: Vec<Member>,
    pub driver: Option<String>,
    pub view: Option<Value>,
    pub view_sequence: u64,
    redirects: BTreeMap<(String, String), String>,
    policy: DriverPolicy,
    released: bool,
}
impl Default for Collaboration {
    fn default() -> Self {
        Self::new(first_connected)
    }
}
impl Collaboration {
    pub fn new(policy: DriverPolicy) -> Self {
        Self {
            rooms: BTreeMap::new(),
            members: Vec::new(),
            driver: None,
            view: None,
            view_sequence: 0,
            redirects: BTreeMap::new(),
            policy,
            released: false,
        }
    }
    pub fn refresh(&mut self, now: u64) {
        self.members.retain(|m| {
            now.saturating_sub(m.touched) <= 45
                && self.rooms.values().any(|r| r.members.contains(&m.token))
        });
        for room in self.rooms.values_mut() {
            room.members
                .retain(|t| self.members.iter().any(|m| &m.token == t));
        }
        let ids: Vec<_> = self.members.iter().map(|m| m.id.clone()).collect();
        if self.members.is_empty() {
            self.released = false;
        }
        if !self.released && !self.driver.as_ref().is_some_and(|id| ids.contains(id)) {
            self.set_driver((self.policy)(&ids));
        }
    }
    fn set_driver(&mut self, id: Option<String>) {
        if id != self.driver {
            self.driver = id;
            self.view = None;
            self.view_sequence += 1;
        }
        for room in self.rooms.values_mut() {
            room.sequence += 1;
        }
    }
    pub fn room(&mut self, path: &str) -> Result<&mut Room> {
        if !self.rooms.contains_key(path) && self.rooms.len() >= 256 {
            return Err(fail(
                ErrorCode::BoundsExceeded,
                "Workspace notebook limit reached",
            ));
        }
        Ok(self.rooms.entry(path.into()).or_default())
    }
    pub fn join(
        &mut self,
        path: &str,
        token: &str,
        new_id: Option<String>,
        now: u64,
    ) -> Result<Value> {
        self.refresh(now);
        self.room(path)?;
        if let Some(id) = new_id {
            if self.members.len() >= 32 || self.members.iter().any(|m| m.token == token) {
                return Err(fail(
                    ErrorCode::BoundsExceeded,
                    "Workspace collaborator limit reached",
                ));
            }
            self.members.push(Member {
                token: token.into(),
                id,
                touched: now,
            });
        } else if !self.members.iter().any(|m| m.token == token) {
            return Err(fail(
                ErrorCode::NotDriver,
                "Workspace session expired; reconnect",
            ));
        }
        self.room(path)?.members.insert(token.into());
        if self.driver.is_none() && !self.released {
            self.set_driver((self.policy)(
                &self
                    .members
                    .iter()
                    .map(|m| m.id.clone())
                    .collect::<Vec<_>>(),
            ));
        }
        for room in self.rooms.values_mut() {
            room.sequence += 1;
        }
        let mut state = self.state(path, token, now)?;
        state["token"] = token.into();
        Ok(state)
    }
    fn member(&mut self, path: &str, token: &str, now: u64) -> Result<String> {
        self.refresh(now);
        if !self.room(path)?.members.contains(token) {
            return Err(fail(
                ErrorCode::NotDriver,
                "Reconnect to join this notebook",
            ));
        }
        let member = self
            .members
            .iter_mut()
            .find(|m| m.token == token)
            .ok_or_else(|| fail(ErrorCode::NotDriver, "Workspace session expired; reconnect"))?;
        member.touched = now;
        Ok(member.id.clone())
    }
    pub fn require_driver(&mut self, path: &str, token: &str, now: u64) -> Result<()> {
        let id = self.member(path, token, now)?;
        if self.driver.as_ref() != Some(&id) {
            return Err(fail(
                ErrorCode::NotDriver,
                "Read-only: only the workspace driver may change notebooks",
            ));
        }
        Ok(())
    }
    pub fn change_driver(&mut self, id: &str) -> Result<()> {
        if self.rooms.values().any(|r| r.active > 0) {
            return Err(fail(
                ErrorCode::ExecutionRejected,
                "Wait for active commands before handoff",
            ));
        }
        if !self.members.iter().any(|m| m.id == id) {
            return Err(fail(
                ErrorCode::InvalidInput,
                "Target collaborator is not connected",
            ));
        }
        self.released = false;
        self.set_driver(Some(id.into()));
        Ok(())
    }
    pub fn release_driver(&mut self, path: &str, token: &str, now: u64) -> Result<()> {
        self.require_driver(path, token, now)?;
        if self.rooms.values().any(|r| r.active > 0) {
            return Err(fail(
                ErrorCode::ExecutionRejected,
                "Wait for active commands before releasing control",
            ));
        }
        self.released = true;
        self.set_driver(None);
        Ok(())
    }
    pub fn claim_driver(&mut self, path: &str, token: &str, now: u64) -> Result<()> {
        let id = self.member(path, token, now)?;
        self.change_driver(&id)
    }
    pub fn state(&mut self, path: &str, token: &str, now: u64) -> Result<Value> {
        let id = self.member(path, token, now)?;
        let room = &self.rooms[path];
        Ok(
            json!({"notebook_path":path,"client_id":id,"driver_id":self.driver,"is_driver":self.driver.as_ref()==Some(&id),"clients":self.members.iter().map(|m| &m.id).collect::<Vec<_>>(),"sequence":room.sequence,"origin":room.origin,"snapshot":room.snapshot}),
        )
    }
    pub fn publish(&mut self, path: &str, token: &str, snapshot: NotebookSnapshot) -> Result<()> {
        let origin = self
            .members
            .iter()
            .find(|m| m.token == token)
            .map(|m| m.id.clone());
        let room = self.room(path)?;
        if room
            .snapshot
            .as_ref()
            .is_some_and(|s| s.revision > snapshot.revision)
        {
            return Ok(());
        }
        room.snapshot = Some(snapshot);
        room.origin = origin;
        room.sequence += 1;
        Ok(())
    }
    pub fn leave(&mut self, path: &str, token: &str, now: u64) -> Result<()> {
        self.member(path, token, now)?;
        self.room(path)?.members.remove(token);
        self.refresh(now);
        Ok(())
    }
    pub fn publish_view(
        &mut self,
        path: &str,
        token: &str,
        target_token: &str,
        view: Value,
        now: u64,
    ) -> Result<()> {
        self.require_driver(path, token, now)?;
        let target = view["notebook_path"]
            .as_str()
            .ok_or_else(|| fail(ErrorCode::InvalidInput, "Missing notebook path"))?;
        self.require_driver(target, target_token, now)?;
        if view["protocol_version"] != 1
            || !view["scroll_fraction"]
                .as_f64()
                .is_some_and(|f| (0.0..=1.0).contains(&f))
            || (!view["selected_cell_id"].is_null()
                && !view["selected_cell_id"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 128))
        {
            return Err(fail(ErrorCode::InvalidInput, "Invalid follow view"));
        }
        if !view["microscope"].is_null() {
            let microscope: notebook_protocol::microscope::MicroscopeTarget =
                serde_json::from_value(view["microscope"].clone()).map_err(|_| {
                    fail(ErrorCode::InvalidInput, "Invalid microscope follow target")
                })?;
            notebook_protocol::microscope::validate_id(&microscope.microscope_id)?;
            if microscope.focus.as_ref().is_some_and(|f| {
                f.step_index >= 64
                    || f.annotation_id.as_ref().is_some_and(|id| {
                        id.is_empty()
                            || id.len() > 64
                            || !id
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    })
            }) {
                return Err(fail(
                    ErrorCode::InvalidInput,
                    "Invalid walkthrough follow focus",
                ));
            }
            if microscope.cell_id.is_empty() || microscope.cell_id.len() > 128 {
                return Err(fail(ErrorCode::InvalidInput, "Invalid microscope cell ID"));
            }
        }
        let normalized = json!({"protocol_version":1,"notebook_path":target,"scroll_fraction":view["scroll_fraction"],"selected_cell_id":view["selected_cell_id"],"microscope":view["microscope"],"driver_id":self.driver});
        if self.view.as_ref() != Some(&normalized) {
            self.view = Some(normalized);
            self.view_sequence += 1;
        }
        Ok(())
    }
    pub fn can_rename(&mut self, old: &str, new: &str) -> Result<()> {
        if self.room(old)?.members.len() > 1
            || self.rooms.contains_key(new)
            || self.redirects.len() >= 256
        {
            return Err(fail(
                ErrorCode::UnsupportedOperation,
                "Close other collaborators or choose a new rename target",
            ));
        }
        Ok(())
    }
    pub fn rename(&mut self, old: &str, new: &str) {
        if let Some(mut room) = self.rooms.remove(old) {
            for target in self.redirects.values_mut() {
                if target == old {
                    *target = new.into();
                }
            }
            for token in &room.members {
                self.redirects
                    .insert((old.into(), token.clone()), new.into());
            }
            room.sequence += 1;
            self.rooms.insert(new.into(), room);
        }
    }
    pub fn event_path(&self, path: &str, token: &str) -> String {
        self.redirects
            .get(&(path.into(), token.into()))
            .cloned()
            .unwrap_or_else(|| path.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_requires_driver_and_claim_is_exclusive_across_notebooks() {
        let mut c = Collaboration::default();
        c.join("one", "a-token", Some("a".into()), 0).unwrap();
        c.join("two", "b-token", Some("b".into()), 0).unwrap();
        assert!(c.release_driver("two", "b-token", 1).is_err());
        c.claim_driver("two", "b-token", 1).unwrap();
        assert_eq!(c.driver.as_deref(), Some("b"));
        c.room("one").unwrap().active = 1;
        assert!(c.release_driver("two", "b-token", 1).is_err());
        c.room("one").unwrap().active = 0;
        c.release_driver("two", "b-token", 2).unwrap();
        c.refresh(3);
        c.join("three", "c-token", Some("c".into()), 3).unwrap();
        assert!(c.driver.is_none());
        assert!(c.require_driver("one", "a-token", 3).is_err());
        assert!(c.claim_driver("two", "b", 3).is_err());
        c.claim_driver("two", "b-token", 4).unwrap();
        c.claim_driver("three", "c-token", 4).unwrap();
        c.require_driver("three", "c-token", 4).unwrap();
        assert!(c.require_driver("one", "a-token", 4).is_err());
    }
    #[test]
    fn workspace_driver_and_private_capabilities() {
        let mut c = Collaboration::default();
        assert_eq!(
            c.join("a", "secret-a", Some("alice".into()), 0).unwrap()["is_driver"],
            true
        );
        assert_eq!(
            c.join("b", "secret-b", Some("bob".into()), 0).unwrap()["is_driver"],
            false
        );
        assert!(c.require_driver("a", "alice", 1).is_err());
        assert!(c.require_driver("b", "secret-b", 1).is_err());
        c.change_driver("bob").unwrap();
        c.require_driver("b", "secret-b", 2).unwrap();
        c.room("b").unwrap().active = 1;
        assert!(c.change_driver("alice").is_err());
        c.refresh(100);
        assert!(c.driver.is_none());
        c.room("b").unwrap().active = 0;
        c.refresh(100);
        assert!(c.driver.is_none());
    }
    #[test]
    fn follow_selection_and_handoff_clear() {
        let mut c = Collaboration::default();
        c.join("a", "t", Some("a".into()), 0).unwrap();
        c.join("b", "t", None, 0).unwrap();
        c.join("a", "u", Some("b".into()), 0).unwrap();
        c.publish_view("a", "t", "t", json!({"protocol_version":1,"notebook_path":"b","scroll_fraction":0.4,"selected_cell_id":"cell"}), 1).unwrap();
        assert_eq!(c.view.as_ref().unwrap()["selected_cell_id"], "cell");
        let view = json!({"protocol_version":1,"notebook_path":"b","scroll_fraction":0.4,"selected_cell_id":"cell","microscope":{"cell_id":"cell","microscope_id":"micro01"}});
        c.publish_view("a", "t", "t", view.clone(), 1).unwrap();
        assert_eq!(c.view.as_ref().unwrap()["microscope"], view["microscope"]);
        let before = c.view.clone();
        let mut malformed = view;
        malformed["microscope"]["microscope_id"] = "../bad".into();
        assert!(c.publish_view("a", "t", "t", malformed, 1).is_err());
        assert_eq!(c.view, before);
        assert!(c.publish_view("a", "u", "t", json!({}), 1).is_err());
        c.change_driver("b").unwrap();
        assert!(c.view.is_none());
    }
}
