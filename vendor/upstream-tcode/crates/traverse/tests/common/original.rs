use std::{path::Path, sync::Arc};

use base64::Engine as _;
use tcode_runtime::pipe::{HostServices, SpawnedHost, spawn_host};
use tcode_traverse::HostMux;

pub fn recorded_host(root: &Path, bytes: &[u8]) -> (SpawnedHost, HostMux, String, String) {
    let store = tcode_services::store::SessionStore::open_at(root.join("store")).unwrap();
    let project = tcode_core::project::Project::from_root(root.to_owned());
    store.upsert_project(&project).unwrap();
    let mut meta =
        tcode_core::project::SessionMeta::new(agent::ProviderKind::Codex, root.to_owned(), None);
    meta.project_id = Some(project.id.clone());
    store.upsert_meta(&meta).unwrap();
    store
        .append_event(
            &meta.id,
            1,
            &agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: "recorded-image".into(),
                parent_item_id: None,
                content: agent::ItemContent::ImageRead {
                    path: "original.png".into(),
                    image: Some(agent::Attachment {
                        media_type: "image/png".into(),
                        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                        source_path: None,
                    }),
                },
            }),
        )
        .unwrap();
    let runtime = spawn_host(store, HostServices::default()).unwrap();
    let mux = HostMux::new(runtime.attachments.clone(), runtime.from_host.clone())
        .with_original_backend(Arc::new(runtime.clone()));
    (runtime, mux, meta.id, project.id)
}

pub fn stop(runtime: SpawnedHost, mux: &HostMux) {
    let attachment = mux.attach(
        tcode_protocol::Principal::Full,
        tcode_client::attachment::AttachmentPolicy::LOCAL,
    );
    let link = tcode_client::HostLink::new(attachment.to_host, attachment.from_host);
    tcode_traverse::block_on(async {
        let pump = tcode_traverse::runtime().spawn({
            let link = link.clone();
            async move { link.pump().await }
        });
        link.shutdown().await.unwrap();
        runtime.to_host.close();
        runtime.stopped.recv().await.unwrap();
        link.close();
        pump.await.unwrap();
    });
}
