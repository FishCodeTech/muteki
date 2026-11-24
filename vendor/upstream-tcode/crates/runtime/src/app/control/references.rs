use super::*;
use control_mcp::{ReferenceParams, ReferenceResponse};
use tcode_core::control::{ReferenceOrigin, ThreadReference, action};

impl AppState {
    /// Only a thread inside the caller's observe scope can be referenced: `authorize_control`
    /// has already held the call to tier observe and the target to the grant's scope.
    pub(super) fn op_reference(
        &mut self,
        caller: &Caller,
        params: ReferenceParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let target = self
            .find_meta(&params.thread_id)
            .ok_or_else(|| format!("unknown thread: {}", params.thread_id))?;
        if target.id == caller.thread_id {
            return Err("a thread always reads its own log; it needs no reference".into());
        }
        let added = self.add_reference(&caller.thread_id, &target, ReferenceOrigin::Model, cx);
        let caller_meta = self.find_meta(&caller.thread_id);
        let boundary_notice = caller_meta
            .as_ref()
            .and_then(|meta| {
                meta.references
                    .iter()
                    .find(|reference| reference.thread_id == target.id)
            })
            .is_some_and(|reference| reference.boundary_notice);
        // The user did not make this reference, so they hear about the boundary when the
        // model makes it rather than at a mention.
        if added && boundary_notice {
            emit_runtime(
                cx,
                RuntimeEvent::Notice(RuntimeNotice::ReferenceBoundaryCrossed {
                    thread: caller.thread_id.clone(),
                    thread_title: caller_meta.map(|meta| meta.title).unwrap_or_default(),
                    referenced: target.id.clone(),
                    title: target.title.clone(),
                }),
            );
        }
        self.audit(
            caller,
            &caller.thread_id,
            action::REFERENCE,
            Some(target.title.clone()),
            cx,
        );
        serde_json::to_value(ReferenceResponse {
            thread_id: target.id,
            title: target.title,
            boundary_notice,
        })
        .map_err(|error| error.to_string())
    }

    /// The user named `thread_id` in `session_id`'s composer. Whether the client should now
    /// tell the user, once, that the new reference crosses a project or provider boundary.
    pub fn add_thread_reference(
        &mut self,
        session_id: &str,
        thread_id: &str,
        cx: &mut HostCx,
    ) -> Result<bool, ProtocolError> {
        let target = self.find_meta(thread_id).ok_or_else(|| ProtocolError {
            code: "unknown_session".into(),
            message: "The referenced thread is no longer available.".into(),
        })?;
        let added = self.add_reference(session_id, &target, ReferenceOrigin::User, cx);
        Ok(added
            && self.find_meta(session_id).is_some_and(|meta| {
                meta.references
                    .iter()
                    .any(|reference| reference.thread_id == target.id && reference.boundary_notice)
            }))
    }

    /// A reference across a project or provider boundary carries a notice; an existing
    /// reference, or one to the thread itself, is left as it is. Whether one was added.
    fn add_reference(
        &mut self,
        session_id: &str,
        target: &SessionMeta,
        by: ReferenceOrigin,
        cx: &mut HostCx,
    ) -> bool {
        let mut added = false;
        self.change_meta(session_id, cx, |meta| {
            if meta.id == target.id
                || meta
                    .references
                    .iter()
                    .any(|reference| reference.thread_id == target.id)
            {
                return;
            }
            meta.references.push(ThreadReference {
                thread_id: target.id.clone(),
                by,
                created_at: now_secs(),
                boundary_notice: meta.project_id != target.project_id
                    || meta.provider != target.provider,
            });
            added = true;
        });
        added
    }

    pub fn remove_thread_reference(&mut self, session_id: &str, thread_id: &str, cx: &mut HostCx) {
        self.change_meta(session_id, cx, |meta| {
            meta.references
                .retain(|reference| reference.thread_id != thread_id);
        });
    }
}
