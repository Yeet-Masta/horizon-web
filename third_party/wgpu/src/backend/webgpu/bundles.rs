//! Render bundles for repeated indirect draws (Horizon's web build).
//!
//! Without Chromium's experimental multi-draw (`MULTI_DRAW`), `multi_draw_indexed_indirect`
//! issues one `drawIndexedIndirect` per draw: the terrain's few hundred a frame, each a call into
//! the browser and a command its GPU process decodes and validates. A bundle reads its draws'
//! parameters from the indirect buffer when it runs, so the same stretch of the buffer drawn
//! with the same pipeline and bindings in an earlier submission is recorded once into a
//! `GPURenderBundle` and from then on replayed: one `executeBundles` for many draws, whatever the
//! stretch holds this frame ([`Pass::draw_indexed_indirect`]). A bundle leaves the pass with no
//! pipeline or bindings: the pass sets them again before its next draw of its own
//! ([`Pass::restore`]).
//!
//! `Device::set_render_bundles(false)` turns it off; the pass then tracks nothing.
use alloc::{rc::Rc, vec::Vec};
use core::cell::{Cell, RefCell};

use super::webgpu_sys::{
    GpuBindGroup, GpuBuffer, GpuDevice, GpuIndexFormat, GpuRenderBundle,
    GpuRenderBundleEncoderDescriptor, GpuRenderPassEncoder, GpuRenderPipeline,
};
use wasm_bindgen::JsCast;

/// A batch's bundle is dropped once this many submissions went by without it.
const KEEP: u64 = 32;
/// Fewer draws than this are issued as they are: a bundle and the bindings set again after it
/// cost about as many calls.
const MIN_DRAWS: u32 = 4;
/// Entries of the indirect buffer per bundle (see [`Pass::draw_indexed_indirect`]).
const CHUNK: u64 = 16;
/// Bytes per indexed indirect draw.
const STRIDE: u64 = 20;
/// More cached stretches than this (bindings that never repeat) start the cache over.
const MAX_BATCHES: usize = 4096;

/// What a render pipeline draws into: a bundle that sets it needs the same.
#[derive(Debug)]
pub(crate) struct Layout {
    pub colors: Vec<Option<wgt::TextureFormat>>,
    pub depth_stencil: Option<wgt::TextureFormat>,
    pub samples: u32,
}

impl Layout {
    pub fn of(desc: &crate::RenderPipelineDescriptor<'_>) -> Rc<Self> {
        Rc::new(Self {
            colors: desc.fragment.as_ref().map_or_else(Vec::new, |f| {
                f.targets
                    .iter()
                    .map(|t| t.as_ref().map(|t| t.format))
                    .collect()
            }),
            depth_stencil: desc.depth_stencil.as_ref().map(|d| d.format),
            samples: desc.multisample.count,
        })
    }
}

/// A device's bundles, by batch.
#[derive(Debug)]
pub(crate) struct Cache {
    device: GpuDevice,
    enabled: Cell<bool>,
    /// The queue's submissions so far.
    submission: Cell<u64>,
    batches: RefCell<hashbrown::HashMap<Vec<u64>, Batch>>,
    /// The batch being looked up, reused.
    key: RefCell<Vec<u64>>,
}

#[derive(Debug)]
struct Batch {
    /// The submission it was last drawn in.
    seen: u64,
    bundle: Option<GpuRenderBundle>,
}

impl Cache {
    pub fn new(device: GpuDevice) -> Rc<Self> {
        Rc::new(Self {
            device,
            enabled: Cell::new(true),
            submission: Cell::new(0),
            batches: RefCell::default(),
            key: RefCell::default(),
        })
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.set(enabled);
        if !enabled {
            self.batches.borrow_mut().clear();
        }
    }

    /// A render pass's tracker: inert while bundles are off.
    pub fn pass(self: &Rc<Self>, depth_read_only: bool, stencil_read_only: bool) -> Pass {
        Pass {
            cache: self.enabled.get().then(|| self.clone()),
            depth_read_only,
            stencil_read_only,
            ..Pass::default()
        }
    }

    /// The queue submitted: batches gone for [`KEEP`] submissions are dropped.
    pub fn submitted(&self) {
        let now = self.submission.get() + 1;
        self.submission.set(now);
        if now % KEEP == 0 {
            self.batches
                .borrow_mut()
                .retain(|_, batch| batch.seen + KEEP > now);
        }
    }
}

/// A render pass's pipeline and bindings, which a bundle sets up again and `executeBundles`
/// clears.
#[derive(Debug, Default)]
pub(crate) struct Pass {
    cache: Option<Rc<Cache>>,
    depth_read_only: bool,
    stencil_read_only: bool,
    pipeline: Option<(u64, GpuRenderPipeline, Rc<Layout>)>,
    groups: Vec<Option<(u64, GpuBindGroup, Vec<u32>)>>,
    vertices: Vec<Option<(u64, GpuBuffer, f64, Option<f64>)>>,
    indices: Option<(u64, GpuBuffer, GpuIndexFormat, f64, Option<f64>)>,
    /// A cached bundle ran: the pass has no pipeline or bindings until [`Pass::restore`].
    cleared: bool,
}

/// Sets `$state`'s pipeline and bindings on `$encoder`, a pass or a bundle encoder.
macro_rules! bind {
    ($encoder:expr, $state:expr) => {{
        let (encoder, state) = (&$encoder, &$state);
        if let Some((_, pipeline, _)) = &state.pipeline {
            encoder.set_pipeline(pipeline);
        }
        for (index, group) in state.groups.iter().enumerate() {
            match group {
                Some((_, group, offsets)) if !offsets.is_empty() => {
                    let _ = encoder
                        .set_bind_group_with_u32_slice_and_f64_and_dynamic_offsets_data_length(
                            index as u32,
                            Some(group),
                            offsets,
                            0.0,
                            offsets.len() as u32,
                        );
                }
                Some((_, group, _)) => encoder.set_bind_group(index as u32, Some(group)),
                None => {}
            }
        }
        for (slot, vertices) in state.vertices.iter().enumerate() {
            match vertices {
                Some((_, buffer, offset, Some(size))) => {
                    encoder.set_vertex_buffer_with_f64_and_f64(
                        slot as u32,
                        Some(buffer),
                        *offset,
                        *size,
                    );
                }
                Some((_, buffer, offset, None)) => {
                    encoder.set_vertex_buffer_with_f64(slot as u32, Some(buffer), *offset);
                }
                None => {}
            }
        }
        match &state.indices {
            Some((_, buffer, format, offset, Some(size))) => {
                encoder.set_index_buffer_with_f64_and_f64(buffer, *format, *offset, *size);
            }
            Some((_, buffer, format, offset, None)) => {
                encoder.set_index_buffer_with_f64(buffer, *format, *offset);
            }
            None => {}
        }
    }};
}

fn put<T>(list: &mut Vec<Option<T>>, index: usize, value: Option<T>) {
    if list.len() <= index {
        list.resize_with(index + 1, || None);
    }
    list[index] = value;
}

impl Pass {
    pub fn tracking(&self) -> bool {
        self.cache.is_some()
    }

    /// `pipeline` is None for one not compiled yet, whose draws are skipped.
    pub fn set_pipeline(&mut self, pipeline: Option<(u64, &GpuRenderPipeline, &Rc<Layout>)>) {
        if self.tracking() {
            self.pipeline = pipeline.map(|(id, p, layout)| (id, p.clone(), layout.clone()));
        }
    }

    pub fn set_bind_group(
        &mut self,
        index: u32,
        group: Option<(u64, &GpuBindGroup)>,
        offsets: &[u32],
    ) {
        if self.tracking() {
            let group = group.map(|(id, group)| (id, group.clone(), offsets.to_vec()));
            put(&mut self.groups, index as usize, group);
        }
    }

    pub fn set_vertex_buffer(
        &mut self,
        slot: u32,
        buffer: Option<(u64, &GpuBuffer)>,
        offset: f64,
        size: Option<f64>,
    ) {
        if self.tracking() {
            let vertices = buffer.map(|(id, buffer)| (id, buffer.clone(), offset, size));
            put(&mut self.vertices, slot as usize, vertices);
        }
    }

    pub fn set_index_buffer(
        &mut self,
        id: u64,
        buffer: &GpuBuffer,
        format: GpuIndexFormat,
        offset: f64,
        size: Option<f64>,
    ) {
        if self.tracking() {
            self.indices = Some((id, buffer.clone(), format, offset, size));
        }
    }

    /// The pass ran bundles of its own: like the pass, it has no pipeline or bindings left.
    pub fn bundles_executed(&mut self) {
        self.pipeline = None;
        self.groups.clear();
        self.vertices.clear();
        self.indices = None;
        self.cleared = false;
    }

    /// Before a draw of the pass's own: the pipeline and bindings again, if a cached bundle
    /// cleared them.
    pub fn restore(&mut self, pass: &GpuRenderPassEncoder) {
        if core::mem::take(&mut self.cleared) {
            bind!(pass, self);
        }
    }

    /// `count` indexed indirect draws from `offset` in `indirect`, through cached bundles where
    /// the same draws were drawn in an earlier submission. False: the caller draws them.
    ///
    /// A bundle reads its draws' parameters when it runs, so what it stands for is a stretch of
    /// the indirect buffer, whatever it holds. The draws are cut at every [`CHUNK`]th entry of
    /// the buffer, and each stretch has its own bundle: a list that grows or shrinks as terrain
    /// loads, or starts a few entries later, still finds the bundles of every whole chunk it
    /// covers. Consecutive bundles go in one `executeBundles`; a stretch drawn for the first
    /// time is drawn as it is.
    pub fn draw_indexed_indirect(
        &mut self,
        pass: &GpuRenderPassEncoder,
        indirect: (u64, &GpuBuffer),
        offset: u64,
        count: u32,
    ) -> bool {
        let Some(cache) = self.cache.clone() else {
            return false;
        };
        if self.pipeline.is_none()
            || count < MIN_DRAWS
            || offset % STRIDE != 0
            || !cache.enabled.get()
        {
            return false;
        }
        let mut key = cache.key.borrow_mut();
        self.state_key(&mut key, indirect.0);
        let base = key.len();
        let now = cache.submission.get();
        let mut batches = cache.batches.borrow_mut();
        if batches.len() > MAX_BATCHES {
            batches.clear();
        }
        let first = offset / STRIDE;
        let end = first + u64::from(count);
        // What to issue, in order: a cached bundle, or draws `start..end` as they are.
        let mut bundles: Vec<GpuRenderBundle> = Vec::new();
        let mut start = first;
        while start < end {
            let stop = ((start / CHUNK + 1) * CHUNK).min(end);
            key.truncate(base);
            key.extend([start, stop - start]);
            let bundle = match batches.get_mut(key.as_slice()) {
                Some(batch) => {
                    if batch.bundle.is_none() && batch.seen != now {
                        batch.bundle = self.record(&cache.device, indirect.1, start, stop);
                    }
                    batch.seen = now;
                    batch.bundle.clone()
                }
                None => {
                    batches.insert(
                        key.clone(),
                        Batch {
                            seen: now,
                            bundle: None,
                        },
                    );
                    None
                }
            };
            match bundle {
                Some(bundle) => bundles.push(bundle),
                None => {
                    self.execute(pass, &mut bundles);
                    self.restore(pass);
                    for i in start..stop {
                        pass.draw_indexed_indirect_with_f64(indirect.1, (i * STRIDE) as f64);
                    }
                }
            }
            start = stop;
        }
        self.execute(pass, &mut bundles);
        true
    }

    /// Runs `bundles`, which leave the pass with no pipeline or bindings.
    fn execute(&mut self, pass: &GpuRenderPassEncoder, bundles: &mut Vec<GpuRenderBundle>) {
        if !bundles.is_empty() {
            pass.execute_bundles(bundles);
            bundles.clear();
            self.cleared = true;
        }
    }

    /// What a bundle of the pass's draws from `indirect` depends on, into `key`.
    fn state_key(&self, key: &mut Vec<u64>, indirect: u64) {
        key.clear();
        key.extend([
            self.pipeline.as_ref().map_or(0, |(id, _, _)| *id),
            indirect,
            u64::from(self.depth_read_only) | u64::from(self.stencil_read_only) << 1,
            self.groups.len() as u64,
        ]);
        for group in &self.groups {
            match group {
                Some((id, _, offsets)) => {
                    key.extend([*id, offsets.len() as u64]);
                    key.extend(offsets.iter().map(|&o| u64::from(o)));
                }
                None => key.push(0),
            }
        }
        key.push(self.vertices.len() as u64);
        for vertices in &self.vertices {
            match vertices {
                Some((id, _, offset, size)) => {
                    key.extend([*id, offset.to_bits(), size.map_or(u64::MAX, f64::to_bits)]);
                }
                None => key.push(0),
            }
        }
        match &self.indices {
            Some((id, _, format, offset, size)) => key.extend([
                *id,
                u64::from(matches!(format, GpuIndexFormat::Uint32)),
                offset.to_bits(),
                size.map_or(u64::MAX, f64::to_bits),
            ]),
            None => key.push(0),
        }
    }

    /// A bundle of the pass's pipeline and bindings and the indexed indirect draws `start..end`
    /// (entries) of `indirect`.
    fn record(
        &self,
        device: &GpuDevice,
        indirect: &GpuBuffer,
        start: u64,
        end: u64,
    ) -> Option<GpuRenderBundle> {
        let (_, _, layout) = self.pipeline.as_ref()?;
        let colors = layout
            .colors
            .iter()
            .map(|format| match format {
                Some(format) => js_sys::JsNullable::wrap(
                    wasm_bindgen::JsValue::from(super::map_texture_format(*format))
                        .unchecked_into::<js_sys::JsString>(),
                ),
                None => js_sys::JsNullable::new(),
            })
            .collect::<Vec<_>>();
        let desc = GpuRenderBundleEncoderDescriptor::new(&colors);
        desc.set_label("Repeated draws");
        if let Some(format) = layout.depth_stencil {
            desc.set_depth_stencil_format(super::map_texture_format(format));
            desc.set_depth_read_only(self.depth_read_only);
            desc.set_stencil_read_only(self.stencil_read_only);
        }
        desc.set_sample_count(layout.samples);
        let encoder = device.create_render_bundle_encoder(&desc).ok()?;
        bind!(encoder, self);
        for i in start..end {
            encoder.draw_indexed_indirect_with_f64(indirect, (i * STRIDE) as f64);
        }
        Some(encoder.finish())
    }
}
