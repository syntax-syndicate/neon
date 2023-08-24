use anyhow::{bail, ensure, Context, Result};
use pageserver_api::models::{LayerResidenceEventReason, LayerResidenceStatus};
use std::{collections::HashMap, sync::Arc};
use tracing::trace;
use utils::{
    id::{TenantId, TimelineId},
    lsn::{AtomicLsn, Lsn},
};

use crate::{
    config::PageServerConf,
    tenant::{
        layer_map::{BatchedUpdates, LayerMap},
        storage_layer::{
            AsLayerDesc, InMemoryLayer, LayerE, PersistentLayerDesc, PersistentLayerKey,
            ResidentLayer,
        },
    },
};

/// Provides semantic APIs to manipulate the layer map.
pub(crate) struct LayerManager {
    layer_map: LayerMap,
    layer_fmgr: LayerFileManager<LayerE>,
}

/// After GC, the layer map changes will not be applied immediately. Users should manually apply the changes after
/// scheduling deletes in remote client.
pub(crate) struct ApplyGcResultGuard<'a>(BatchedUpdates<'a>);

impl ApplyGcResultGuard<'_> {
    pub(crate) fn flush(self) {
        self.0.flush();
    }
}

impl LayerManager {
    pub(crate) fn create() -> Self {
        Self {
            layer_map: LayerMap::default(),
            layer_fmgr: LayerFileManager::new(),
        }
    }

    pub(crate) fn get_from_desc(&self, desc: &PersistentLayerDesc) -> Arc<LayerE> {
        self.layer_fmgr.get_from_desc(desc)
    }

    /// Get an immutable reference to the layer map.
    ///
    /// We expect users only to be able to get an immutable layer map. If users want to make modifications,
    /// they should use the below semantic APIs. This design makes us step closer to immutable storage state.
    pub(crate) fn layer_map(&self) -> &LayerMap {
        &self.layer_map
    }

    /// Called from `load_layer_map`. Initialize the layer manager with:
    /// 1. all on-disk layers
    /// 2. next open layer (with disk disk_consistent_lsn LSN)
    pub(crate) fn initialize_local_layers(
        &mut self,
        on_disk_layers: Vec<Arc<LayerE>>,
        next_open_layer_at: Lsn,
    ) {
        let mut updates = self.layer_map.batch_update();
        for layer in on_disk_layers {
            Self::insert_historic_layer(layer, &mut updates, &mut self.layer_fmgr);
        }
        updates.flush();
        self.layer_map.next_open_layer_at = Some(next_open_layer_at);
    }

    /// Initialize when creating a new timeline, called in `init_empty_layer_map`.
    pub(crate) fn initialize_empty(&mut self, next_open_layer_at: Lsn) {
        self.layer_map.next_open_layer_at = Some(next_open_layer_at);
    }

    /// Open a new writable layer to append data if there is no open layer, otherwise return the current open layer,
    /// called within `get_layer_for_write`.
    pub(crate) fn get_layer_for_write(
        &mut self,
        lsn: Lsn,
        last_record_lsn: Lsn,
        conf: &'static PageServerConf,
        timeline_id: TimelineId,
        tenant_id: TenantId,
    ) -> Result<Arc<InMemoryLayer>> {
        ensure!(lsn.is_aligned());

        ensure!(
            lsn > last_record_lsn,
            "cannot modify relation after advancing last_record_lsn (incoming_lsn={}, last_record_lsn={})",
            lsn,
            last_record_lsn,
        );

        // Do we have a layer open for writing already?
        let layer = if let Some(open_layer) = &self.layer_map.open_layer {
            if open_layer.get_lsn_range().start > lsn {
                bail!(
                    "unexpected open layer in the future: open layers starts at {}, write lsn {}",
                    open_layer.get_lsn_range().start,
                    lsn
                );
            }

            Arc::clone(open_layer)
        } else {
            // No writeable layer yet. Create one.
            let start_lsn = self
                .layer_map
                .next_open_layer_at
                .context("No next open layer found")?;

            trace!(
                "creating in-memory layer at {}/{} for record at {}",
                timeline_id,
                start_lsn,
                lsn
            );

            let new_layer = InMemoryLayer::create(conf, timeline_id, tenant_id, start_lsn)?;
            let layer = Arc::new(new_layer);

            self.layer_map.open_layer = Some(layer.clone());
            self.layer_map.next_open_layer_at = None;

            layer
        };

        Ok(layer)
    }

    /// Called from `freeze_inmem_layer`, returns true if successfully frozen.
    pub(crate) async fn try_freeze_in_memory_layer(
        &mut self,
        Lsn(last_record_lsn): Lsn,
        last_freeze_at: &AtomicLsn,
    ) {
        let end_lsn = Lsn(last_record_lsn + 1);

        if let Some(open_layer) = &self.layer_map.open_layer {
            let open_layer_rc = Arc::clone(open_layer);
            // Does this layer need freezing?
            open_layer.freeze(end_lsn).await;

            // The layer is no longer open, update the layer map to reflect this.
            // We will replace it with on-disk historics below.
            self.layer_map.frozen_layers.push_back(open_layer_rc);
            self.layer_map.open_layer = None;
            self.layer_map.next_open_layer_at = Some(end_lsn);
            last_freeze_at.store(end_lsn);
        }
    }

    /// Add image layers to the layer map, called from `create_image_layers`.
    pub(crate) fn track_new_image_layers(
        &mut self,
        image_layers: &[ResidentLayer],
        metrics: &crate::metrics::TimelineMetrics,
    ) {
        let mut updates = self.layer_map.batch_update();
        for layer in image_layers {
            metrics
                .resident_physical_size_gauge
                .add(layer.layer_desc().file_size);
            layer.access_stats().record_residence_event(
                LayerResidenceStatus::Resident,
                LayerResidenceEventReason::LayerCreate,
            );
            Self::insert_historic_layer(layer.as_ref().clone(), &mut updates, &mut self.layer_fmgr);
        }
        updates.flush();
    }

    /// Flush a frozen layer and add the written delta layer to the layer map.
    pub(crate) fn finish_flush_l0_layer(
        &mut self,
        delta_layer: Option<&ResidentLayer>,
        frozen_layer_for_check: &Arc<InMemoryLayer>,
        metrics: &crate::metrics::TimelineMetrics,
    ) {
        let inmem = self
            .layer_map
            .frozen_layers
            .pop_front()
            .expect("there must be a inmem layer to flush");

        // Only one task may call this function at a time (for this
        // timeline). If two tasks tried to flush the same frozen
        // layer to disk at the same time, that would not work.
        assert_eq!(Arc::as_ptr(&inmem), Arc::as_ptr(frozen_layer_for_check));

        if let Some(l) = delta_layer {
            let mut updates = self.layer_map.batch_update();
            l.access_stats().record_residence_event(
                LayerResidenceStatus::Resident,
                LayerResidenceEventReason::LayerCreate,
            );
            let sz = l.layer_desc().file_size;
            metrics.resident_physical_size_gauge.add(sz);
            metrics.num_persistent_files_created.inc_by(1);
            metrics.persistent_bytes_written.inc_by(sz);

            Self::insert_historic_layer(l.as_ref().clone(), &mut updates, &mut self.layer_fmgr);
            updates.flush();
        }
    }

    /// Called when compaction is completed.
    pub(crate) fn finish_compact_l0(
        &mut self,
        layer_removal_cs: &Arc<tokio::sync::OwnedMutexGuard<()>>,
        compact_from: Vec<Arc<LayerE>>,
        compact_to: &[ResidentLayer],
        metrics: &crate::metrics::TimelineMetrics,
    ) -> Result<()> {
        let mut updates = self.layer_map.batch_update();
        for l in compact_to {
            l.access_stats().record_residence_event(
                LayerResidenceStatus::Resident,
                LayerResidenceEventReason::LayerCreate,
            );
            metrics
                .resident_physical_size_gauge
                .add(l.layer_desc().file_size);

            Self::insert_historic_layer(l.as_ref().clone(), &mut updates, &mut self.layer_fmgr);
        }
        for l in compact_from {
            // NB: the layer file identified by descriptor `l` is guaranteed to be present
            // in the LayerFileManager because compaction kept holding `layer_removal_cs` the entire
            // time, even though we dropped `Timeline::layers` inbetween.
            Self::delete_historic_layer(layer_removal_cs, l, &mut updates, &mut self.layer_fmgr)?;
        }
        updates.flush();
        Ok(())
    }

    /// Called when garbage collect the timeline. Returns a guard that will apply the updates to the layer map.
    pub(crate) fn finish_gc_timeline(
        &mut self,
        layer_removal_cs: &Arc<tokio::sync::OwnedMutexGuard<()>>,
        gc_layers: Vec<Arc<LayerE>>,
    ) -> Result<ApplyGcResultGuard> {
        let mut updates = self.layer_map.batch_update();
        for doomed_layer in gc_layers {
            Self::delete_historic_layer(
                layer_removal_cs,
                doomed_layer,
                &mut updates,
                &mut self.layer_fmgr,
            )?;
        }
        Ok(ApplyGcResultGuard(updates))
    }

    /// Helper function to insert a layer into the layer map and file manager.
    fn insert_historic_layer(
        layer: Arc<LayerE>,
        updates: &mut BatchedUpdates<'_>,
        mapping: &mut LayerFileManager<LayerE>,
    ) {
        updates.insert_historic(layer.layer_desc().clone());
        mapping.insert(layer);
    }

    /// Removes the layer from local FS (if present) and from memory.
    /// Remote storage is not affected by this operation.
    fn delete_historic_layer(
        // we cannot remove layers otherwise, since gc and compaction will race
        _layer_removal_cs: &Arc<tokio::sync::OwnedMutexGuard<()>>,
        layer: Arc<LayerE>,
        updates: &mut BatchedUpdates<'_>,
        mapping: &mut LayerFileManager<LayerE>,
    ) -> anyhow::Result<()> {
        let desc = layer.layer_desc();

        // TODO Removing from the bottom of the layer map is expensive.
        //      Maybe instead discard all layer map historic versions that
        //      won't be needed for page reconstruction for this timeline,
        //      and mark what we can't delete yet as deleted from the layer
        //      map index without actually rebuilding the index.
        updates.remove_historic(desc);
        mapping.remove(&layer);
        layer.garbage_collect();

        Ok(())
    }

    pub(crate) fn contains(&self, layer: &Layer) -> bool {
        self.layer_fmgr.contains(layer)
    }
}

pub(crate) struct LayerFileManager<T>(HashMap<PersistentLayerKey, T>);

impl<T: AsLayerDesc + Clone> LayerFileManager<T> {
    fn get_from_desc(&self, desc: &PersistentLayerDesc) -> T {
        // The assumption for the `expect()` is that all code maintains the following invariant:
        // A layer's descriptor is present in the LayerMap => the LayerFileManager contains a layer for the descriptor.
        self.0
            .get(&desc.key())
            .with_context(|| format!("get layer from desc: {}", desc.filename()))
            .expect("not found")
            .clone()
    }

    pub(crate) fn insert(&mut self, layer: T) {
        let present = self.0.insert(layer.layer_desc().key(), layer.clone());
        if present.is_some() && cfg!(debug_assertions) {
            panic!("overwriting a layer: {:?}", layer.layer_desc())
        }
    }

    pub(crate) fn contains(&self, layer: &T) -> bool {
        self.0.contains_key(&layer.layer_desc().key())
    }

    pub(crate) fn new() -> Self {
        Self(HashMap::new())
    }

    pub(crate) fn remove(&mut self, layer: &T) {
        let present = self.0.remove(&layer.layer_desc().key());
        if present.is_none() && cfg!(debug_assertions) {
            panic!(
                "removing layer that is not present in layer mapping: {:?}",
                layer.layer_desc()
            )
        }
    }
}
