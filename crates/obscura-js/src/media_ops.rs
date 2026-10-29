//! Page-owned video decoder resources. All realms share this resource table
//! and budget; codec work leaves the isolate under the existing media permit.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};

use deno_core::{op2, JsBuffer, OpState, Resource};
use deno_error::JsErrorBox;
use serde::Serialize;
use obscura_dom::NodeId;

use crate::media::{self, MediaDecoder};
use crate::ops::{ObscuraState, RealmStates, SharedState, MEDIA_JOB};

const MAX_DECODERS: usize = 16;

#[derive(Default)]
struct MediaBudget {
    bytes: AtomicUsize,
    decoders: AtomicUsize,
}

#[derive(Default)]
struct MediaResources {
    budget: Arc<MediaBudget>,
    decoders: RefCell<HashMap<u32, Weak<DecoderResource>>>,
}

struct Reservation {
    budget: Arc<MediaBudget>,
    bytes: usize,
}

fn error(message: &str) -> JsErrorBox { JsErrorBox::generic(message.to_owned()) }

impl Reservation {
    fn new(budget: Arc<MediaBudget>, bytes: usize) -> Result<Self, JsErrorBox> {
        budget.decoders.fetch_update(Ordering::AcqRel, Ordering::Acquire,
            |count| (count < MAX_DECODERS).then_some(count + 1))
            .map_err(|_| error("media_decoder_limit_exceeded"))?;
        let mut reservation = Self { budget, bytes: 0 };
        reservation.grow(bytes)?;
        Ok(reservation)
    }

    fn grow(&mut self, additional: usize) -> Result<(), JsErrorBox> {
        self.budget.bytes.fetch_update(Ordering::AcqRel, Ordering::Acquire, |bytes| {
            bytes.checked_add(additional).filter(|total| *total <= media::MAX_BYTES)
        }).map_err(|_| error("media_buffer_limit_exceeded"))?;
        self.bytes += additional;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget.decoders.fetch_sub(1, Ordering::AcqRel);
    }
}

struct DecoderJob {
    decoder: MediaDecoder,
    // Keep input and two output windows charged even when a cancelled job is
    // still finishing on a blocking worker. Codec reference planes are bounded
    // separately by dimensions and the maximum number of decoder resources.
    _reservation: Reservation,
}

struct DocumentOwner {
    state: Weak<RefCell<ObscuraState>>,
    frame: u32,
    generation: u64,
    node: NodeId,
}

impl DocumentOwner {
    fn new(state: &OpState, frame: u32, node: u32) -> Result<Self, JsErrorBox> {
        let shared = if frame == 0 {
            state.borrow::<SharedState>().clone()
        } else {
            state.borrow::<Rc<RefCell<RealmStates>>>().borrow().by_frame_id(frame)
                .ok_or_else(|| error("media_document_unavailable"))?
        };
        let node = NodeId::new(node);
        let document = shared.borrow();
        if !document.dom.as_ref().and_then(|dom| dom.get_node(node)).is_some_and(|node| {
            node.as_element().is_some_and(|name| matches!(name.local.as_ref(), "video" | "audio"))
        }) { return Err(error("media_element_unavailable")); }
        let generation = document.document_generation;
        drop(document);
        let owner = Self { state: Rc::downgrade(&shared), frame, generation, node };
        if !owner.live(state) { return Err(error("media_document_unavailable")); }
        Ok(owner)
    }

    fn live(&self, state: &OpState) -> bool {
        let Some(owner) = self.state.upgrade() else { return false; };
        if owner.borrow().document_generation != self.generation { return false; }
        self.frame == 0 || state.borrow::<Rc<RefCell<RealmStates>>>().borrow()
            .frame_timers_active(self.frame) != Some(false)
    }
}

struct DecoderResource {
    job: RefCell<Option<DecoderJob>>,
    cancel: Arc<AtomicBool>,
    owner: DocumentOwner,
}

impl DecoderResource {
    fn release(&self) {
        self.cancel.store(true, Ordering::Release);
        self.job.borrow_mut().take();
    }

    // Only document retirement can clear by node ID alone. A late close from
    // an older source must not erase a newer source's surface on the same node.
    fn retire_surface(&self) {
        if let Some(state) = self.owner.state.upgrade() {
            if let Ok(mut state) = state.try_borrow_mut() {
                if state.document_generation == self.owner.generation {
                    crate::ops::release_media_surface(&mut state, self.owner.node);
                }
            }
        }
    }
}

impl Resource for DecoderResource {
    fn name(&self) -> std::borrow::Cow<'_, str> { "mediaDecoder".into() }
    fn close(self: Rc<Self>) { self.release(); }
}

impl Drop for DecoderResource {
    fn drop(&mut self) { self.release(); }
}

// The DOM retirement boundary has only a shared OpState borrow. Release the
// expensive data immediately; the next media open reaps the closed table slots.
pub(crate) fn close_inactive(state: &OpState) {
    let Some(resources) = state.try_borrow::<Rc<MediaResources>>() else { return; };
    for decoder in resources.decoders.borrow().values().filter_map(Weak::upgrade) {
        if !decoder.owner.live(state) { decoder.release(); decoder.retire_surface(); }
    }
}

fn resources(state: &mut OpState) -> Rc<MediaResources> {
    close_inactive(state);
    if state.try_borrow::<Rc<MediaResources>>().is_none() {
        state.put(Rc::new(MediaResources::default()));
    }
    let resources = state.borrow::<Rc<MediaResources>>().clone();
    resources.decoders.borrow_mut().retain(|rid, weak| {
        let closed = weak.upgrade().is_none_or(|resource| resource.cancel.load(Ordering::Acquire));
        if closed {
            if let Ok(resource) = state.resource_table.take::<DecoderResource>(*rid) { resource.close(); }
        }
        !closed
    });
    resources
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DecoderInfo {
    rid: u32,
    width: u32,
    height: u32,
    times: Vec<f64>,
    duration_ms: f64,
}

#[op2]
#[serde]
pub(crate) async fn op_media_open(
    state: Rc<RefCell<OpState>>,
    #[buffer] bytes: JsBuffer,
    frame: u32,
    node: u32,
) -> Result<DecoderInfo, JsErrorBox> {
    if bytes.is_empty() || bytes.len() > media::MAX_BYTES {
        return Err(error("media_buffer_limit_exceeded"));
    }
    let (owner, reservation) = {
        let mut state = state.borrow_mut();
        let owner = DocumentOwner::new(&state, frame, node)?;
        let reservation = Reservation::new(resources(&mut state).budget.clone(), bytes.len())?;
        (owner, reservation)
    };
    let permit = MEDIA_JOB.acquire().await.map_err(|_| error("media_worker_unavailable"))?;
    if !owner.live(&state.borrow()) { return Err(error("media_aborted")); }
    let mut input = Vec::new();
    input.try_reserve_exact(bytes.len()).map_err(|_| error("media_allocation_failed"))?;
    input.extend_from_slice(&bytes);
    let mut job = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        MediaDecoder::open(input).map(|decoder| DecoderJob { decoder, _reservation: reservation })
    }).await.map_err(|_| error("media_worker_failed"))?
        .map_err(|failure| error(&failure.to_string()))?;
    if !owner.live(&state.borrow()) { return Err(error("media_aborted")); }
    let window = media::frame_bytes(job.decoder.width(), job.decoder.height())
        .map_err(|failure| error(&failure.to_string()))?;
    job._reservation.grow(window.checked_mul(2).ok_or_else(|| error("media_buffer_limit_exceeded"))?)?;
    let mut info = DecoderInfo { rid: 0, width: job.decoder.width(), height: job.decoder.height(),
        times: job.decoder.timestamps().to_vec(), duration_ms: job.decoder.duration_ms() };
    let resource = Rc::new(DecoderResource {
        job: RefCell::new(Some(job)), cancel: Arc::new(AtomicBool::new(false)), owner,
    });
    let mut state = state.borrow_mut();
    info.rid = state.resource_table.add_rc(resource.clone());
    state.borrow::<Rc<MediaResources>>().decoders.borrow_mut().insert(info.rid, Rc::downgrade(&resource));
    Ok(info)
}

// Empty output means this bounded decode step has not reached the requested
// frame yet. A non-empty output is exactly width*height*4 RGBA bytes.
#[op2]
#[buffer]
pub(crate) async fn op_media_frame(
    state: Rc<RefCell<OpState>>,
    rid: u32,
    index: u32,
) -> Result<Vec<u8>, JsErrorBox> {
    let resource = state.borrow().resource_table.get::<DecoderResource>(rid)
        .map_err(|_| error("media_decoder_closed"))?;
    if resource.cancel.load(Ordering::Acquire) || !resource.owner.live(&state.borrow()) {
        resource.release();
        return Err(error("media_aborted"));
    }
    let mut job = resource.job.borrow_mut().take().ok_or_else(|| error("media_decoder_busy"))?;
    let permit = MEDIA_JOB.acquire().await.map_err(|_| error("media_worker_unavailable"))?;
    let cancel = resource.cancel.clone();
    let (job, result) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = job.decoder.decode_to(index as usize, &cancel);
        (job, result)
    }).await.map_err(|_| error("media_worker_failed"))?;
    if resource.cancel.load(Ordering::Acquire) || !resource.owner.live(&state.borrow()) {
        resource.release();
        return Err(error("media_aborted"));
    }
    *resource.job.borrow_mut() = Some(job);
    result.map(|frame| frame.unwrap_or_default()).map_err(|failure| error(&failure.to_string()))
}

#[op2(fast)]
pub(crate) fn op_media_close(state: &mut OpState, rid: u32) -> bool {
    let Ok(resource) = state.resource_table.take::<DecoderResource>(rid) else { return false; };
    resource.close();
    if let Some(resources) = state.try_borrow::<Rc<MediaResources>>() {
        resources.decoders.borrow_mut().remove(&rid);
    }
    true
}
