// This source is included only by the optional media feature. Codecs remain
// native operations; these objects own browser state, events and clocks.
(function installMedia() {
  const MAX_BYTES = 128 * 1024 * 1024;
  const MAX_FRAMES = 600;
  const MAX_DURATION = 120000;
  const trackStates = new WeakMap();
  const streamStates = new WeakMap();
  const recorderStates = new WeakMap();
  const videoStates = new WeakMap();
  const blobUrls = new Map();
  const sources = new WeakMap();
  const trackKey = {};
  let decodedBytes = 0;
  let recordingBytes = 0;
  const queue = callback => setTimeout(callback, 0);
  const now = () => performance.now();
  const failure = (message, name = 'NotSupportedError') => new DOMException(message, name);
  const emit = (target, type) => target.dispatchEvent(new Event(type));

  function handlers(prototype, names) {
    for (const name of names) {
      const values = new WeakMap();
      Object.defineProperty(prototype, 'on' + name, {
        configurable: true, enumerable: true,
        get() { return values.get(this) || null; },
        set(callback) {
          const previous = values.get(this);
          if (previous) this.removeEventListener(name, previous);
          if (typeof callback === 'function') {
            values.set(this, callback);
            this.addEventListener(name, callback);
          } else values.delete(this);
        },
      });
    }
  }

  function capture(source, selectedTrack = null) {
    const context = source.canvas.getContext('2d');
    if (!context || !context._w || !context._h) return;
    for (const track of selectedTrack ? [selectedTrack] : [...source.tracks]) {
      const state = trackStates.get(track);
      if (state.ended) continue;
      for (const subscriber of [...state.subscribers]) subscriber.frame(context, state.enabled);
    }
  }

  class MediaStreamTrack extends EventTarget {
    constructor(key, source) {
      super();
      if (key !== trackKey) throw new TypeError('Illegal constructor');
      const state = { source, ended: false, enabled: true, subscribers: new Set() };
      trackStates.set(this, state);
      source.tracks.add(this);
      Object.defineProperty(this, 'id', { value: crypto.randomUUID(), enumerable: true });
    }
    get kind() { return 'video'; }
    get label() { return 'Canvas'; }
    get muted() { return false; }
    get enabled() { return trackStates.get(this).enabled; }
    set enabled(value) { trackStates.get(this).enabled = !!value; }
    get readyState() { return trackStates.get(this).ended ? 'ended' : 'live'; }
    stop() {
      const state = trackStates.get(this);
      if (state.ended) return;
      state.ended = true;
      state.source.tracks.delete(this);
      if (!state.source.tracks.size) {
        clearInterval(state.source.timer);
        state.source.timer = null;
      }
      for (const subscriber of [...state.subscribers]) subscriber.end();
      state.subscribers.clear();
    }
    clone() {
      const state = trackStates.get(this);
      const copy = new CanvasCaptureMediaStreamTrack(trackKey, state.source);
      copy.enabled = state.enabled;
      if (state.ended) copy.stop();
      return copy;
    }
    getSettings() {
      const source = trackStates.get(this).source;
      return { width: source.canvas.width, height: source.canvas.height, frameRate: source.frameRate };
    }
    getConstraints() { return {}; }
    applyConstraints(constraints = {}) {
      if (Object.keys(constraints).length) return Promise.reject(failure('Canvas track constraints are unsupported.', 'OverconstrainedError'));
      return Promise.resolve();
    }
  }
  class CanvasCaptureMediaStreamTrack extends MediaStreamTrack {
    get canvas() { return trackStates.get(this).source.canvas; }
    requestFrame() {
      const state = trackStates.get(this);
      if (!state.ended) capture(state.source, this);
    }
  }
  class MediaStream extends EventTarget {
    constructor(tracks = []) {
      super();
      if (streamStates.has(tracks)) tracks = tracks.getTracks();
      const list = Array.from(tracks);
      if (!list.every(track => trackStates.has(track))) throw new TypeError('Expected MediaStreamTrack objects');
      streamStates.set(this, new Set(list));
      Object.defineProperty(this, 'id', { value: crypto.randomUUID(), enumerable: true });
    }
    get active() { return this.getTracks().some(track => track.readyState === 'live'); }
    getTracks() { return [...streamStates.get(this)]; }
    getVideoTracks() { return this.getTracks(); }
    getAudioTracks() { return []; }
    getTrackById(id) { return this.getTracks().find(track => track.id === String(id)) || null; }
    addTrack(track) {
      if (!trackStates.has(track)) throw new TypeError('Expected a MediaStreamTrack');
      streamStates.get(this).add(track);
    }
    removeTrack(track) {
      if (!trackStates.has(track)) throw new TypeError('Expected a MediaStreamTrack');
      streamStates.get(this).delete(track);
    }
    clone() { return new MediaStream(this.getTracks().map(track => track.clone())); }
  }
  globalThis.MediaStreamTrack = MediaStreamTrack;
  globalThis.CanvasCaptureMediaStreamTrack = CanvasCaptureMediaStreamTrack;
  globalThis.MediaStream = MediaStream;
  handlers(MediaStreamTrack.prototype, ['ended', 'mute', 'unmute']);
  handlers(MediaStream.prototype, ['addtrack', 'removetrack']);

  HTMLCanvasElement.prototype.captureStream = function captureStream(frameRate = 30) {
    frameRate = Number(frameRate);
    if (!Number.isFinite(frameRate) || frameRate < 0) throw failure('Frame rate must be finite and nonnegative.', 'NotSupportedError');
    if (typeof __obscuraCore.ops.op_media_encode_webm !== 'function') throw failure('Media encoding is unavailable.');
    let source = sources.get(this);
    if (!source || !source.tracks.size) {
      source = { canvas: this, frameRate, tracks: new Set(), timer: null };
      sources.set(this, source);
    } else if (source.frameRate !== frameRate) {
      // Each captureStream call has its own requested sampling cadence.
      source = { canvas: this, frameRate, tracks: new Set(), timer: null };
    }
    const track = new CanvasCaptureMediaStreamTrack(trackKey, source);
    if (frameRate > 0 && source.timer === null) {
      source.timer = setInterval(() => capture(source), Math.max(1, 1000 / frameRate));
    }
    queue(() => { if (track.readyState === 'live') capture(source, track); });
    return new MediaStream([track]);
  };

  function supportedType(value) {
    const type = String(value).trim().toLowerCase();
    return type === '' || /^video\/webm(?:\s*;\s*codecs\s*=\s*(?:vp8|"vp8"))?$/.test(type);
  }
  class BlobEvent extends Event {
    constructor(type, options = {}) {
      super(type, options);
      if (!(options.data instanceof Blob)) throw new TypeError('BlobEvent data must be a Blob');
      Object.defineProperties(this, {
        data: { value: options.data, enumerable: true },
        timecode: { value: Number(options.timecode || 0), enumerable: true },
      });
    }
  }
  globalThis.BlobEvent = BlobEvent;

  function recorderError(recorder, state, error) {
    if (state.finished) return;
    state.finished = true;
    state.state = 'inactive';
    clearInterval(state.timer);
    trackStates.get(state.track).subscribers.delete(state.subscriber);
    state.frames = [];
    recordingBytes -= state.retainedBytes;
    state.retainedBytes = 0;
    const event = new Event('error');
    event.error = error instanceof DOMException ? error : failure(String(error.message || error), 'UnknownError');
    recorder.dispatchEvent(event);
    recorder.dispatchEvent(new BlobEvent('dataavailable', { data: new Blob([], { type: state.mimeType }) }));
    emit(recorder, 'stop');
  }
  function flush(recorder, state) {
    const frames = state.frames;
    state.frames = [];
    const bytes = frames.reduce((total, frame) => total + frame.bytes.length, 0);
    state.chain = state.chain.then(async () => {
      if (state.finished) return;
      let encoded = new Uint8Array();
      if (frames.length) {
        const input = new Uint8Array(bytes);
        let offset = 0;
        for (const frame of frames) { input.set(frame.bytes, offset); offset += frame.bytes.length; }
        encoded = await __obscuraCore.ops.op_media_encode_webm(
          state.width, state.height, JSON.stringify(frames.map(frame => frame.time)),
          0, !state.headerSent, input);
        if (encoded.length + state.encodedBytes > MAX_BYTES) throw failure('The recording exceeds the encoded byte limit.', 'QuotaExceededError');
        state.encodedBytes += encoded.length;
        state.headerSent = true;
        state.retainedBytes -= bytes;
        recordingBytes -= bytes;
      }
      if (!state.finished) recorder.dispatchEvent(new BlobEvent('dataavailable', {
        data: new Blob([encoded], { type: state.mimeType }),
        timecode: frames.length ? frames[0].time : state.lastTime,
      }));
    }).then(undefined, error => recorderError(recorder, state, error));
    return state.chain;
  }
  class MediaRecorder extends EventTarget {
    constructor(stream, options = {}) {
      super();
      if (!streamStates.has(stream)) throw new TypeError('Expected a MediaStream');
      if (!supportedType(options.mimeType || '')) throw failure('Only VP8 WebM recording is supported.');
      recorderStates.set(this, { stream, mimeType: 'video/webm;codecs=vp8', state: 'inactive' });
    }
    static isTypeSupported(type) {
      return typeof __obscuraCore.ops.op_media_encode_webm === 'function' && supportedType(type);
    }
    get stream() { return recorderStates.get(this).stream; }
    get state() { return recorderStates.get(this).state; }
    get mimeType() { return recorderStates.get(this).mimeType; }
    start(timeslice) {
      const previous = recorderStates.get(this);
      if (previous.state !== 'inactive') throw failure('The recorder is already active.', 'InvalidStateError');
      if (timeslice !== undefined && (!Number.isFinite(Number(timeslice)) || Number(timeslice) < 0)) throw new TypeError('Timeslice must be finite and nonnegative');
      const tracks = previous.stream.getVideoTracks();
      if (tracks.length !== 1 || tracks[0].readyState !== 'live') throw failure('Recording requires one live canvas video track.');
      const state = { stream: previous.stream, mimeType: previous.mimeType, state: 'recording',
        track: tracks[0], frames: [], retainedBytes: 0, frameCount: 0, lastTime: -1,
        startTime: now(), pausedAt: 0, pauseDuration: 0, headerSent: false, finished: false,
        width: 0, height: 0, timer: null, encodedBytes: 0 };
      recorderStates.set(this, state);
      state.chain = new Promise(resolve => queue(() => { emit(this, 'start'); resolve(); }));
      state.subscriber = {
        frame: (context, enabled) => {
          if (state.state !== 'recording' || state.finished) return;
          const time = Math.max(state.lastTime + 1, Math.round(now() - state.startTime - state.pauseDuration));
          const size = context._w * context._h * 4;
          if (context._w > 4096 || context._h > 4096 || context._w * context._h > 4 * 1024 * 1024
              || size + recordingBytes > MAX_BYTES || state.frameCount >= MAX_FRAMES || time > MAX_DURATION) {
            state.chain = state.chain.then(() => recorderError(this, state, failure('Recording exceeds the media limits.', 'QuotaExceededError')));
            state.state = 'inactive';
            return;
          }
          if (state.width && (state.width !== context._w || state.height !== context._h)) {
            state.chain = state.chain.then(() => recorderError(this, state, failure('Canvas size changed during recording.', 'InvalidModificationError')));
            state.state = 'inactive';
            return;
          }
          state.width = context._w; state.height = context._h;
          const bytes = enabled ? new Uint8Array(context._buf) : new Uint8Array(size);
          if (!enabled) for (let i = 3; i < bytes.length; i += 4) bytes[i] = 255;
          state.frames.push({ time, bytes });
          state.frameCount++; state.lastTime = time; state.retainedBytes += size; recordingBytes += size;
        },
        end: () => { if (state.state !== 'inactive') this.stop(); },
      };
      trackStates.get(state.track).subscribers.add(state.subscriber);
      if (timeslice !== undefined) state.timer = setInterval(() => {
        if (state.state === 'recording') flush(this, state);
      }, Math.max(1, Number(timeslice)));
    }
    stop() {
      const state = recorderStates.get(this);
      if (state.state === 'inactive') return;
      state.state = 'inactive';
      clearInterval(state.timer);
      trackStates.get(state.track).subscribers.delete(state.subscriber);
      flush(this, state).then(() => {
        if (!state.finished) { state.finished = true; emit(this, 'stop'); }
      });
    }
    pause() {
      const state = recorderStates.get(this);
      if (state.state === 'inactive') throw failure('The recorder is inactive.', 'InvalidStateError');
      if (state.state === 'paused') return;
      state.state = 'paused'; state.pausedAt = now();
      state.chain = state.chain.then(() => emit(this, 'pause'));
    }
    resume() {
      const state = recorderStates.get(this);
      if (state.state === 'inactive') throw failure('The recorder is inactive.', 'InvalidStateError');
      if (state.state === 'recording') return;
      state.pauseDuration += now() - state.pausedAt;
      state.state = 'recording';
      state.chain = state.chain.then(() => emit(this, 'resume'));
    }
    requestData() {
      const state = recorderStates.get(this);
      if (state.state === 'inactive') throw failure('The recorder is inactive.', 'InvalidStateError');
      flush(this, state);
    }
  }
  globalThis.MediaRecorder = MediaRecorder;
  handlers(MediaRecorder.prototype, ['start', 'stop', 'pause', 'resume', 'dataavailable', 'error']);

  const createObjectURL = URL.createObjectURL;
  const revokeObjectURL = URL.revokeObjectURL;
  URL.createObjectURL = function(blob) {
    const url = createObjectURL.apply(this, arguments);
    blobUrls.set(url, blob);
    return url;
  };
  URL.revokeObjectURL = function(url) {
    blobUrls.delete(String(url));
    return revokeObjectURL.apply(this, arguments);
  };

  class MediaError {
    static MEDIA_ERR_ABORTED = 1;
    static MEDIA_ERR_NETWORK = 2;
    static MEDIA_ERR_DECODE = 3;
    static MEDIA_ERR_SRC_NOT_SUPPORTED = 4;
    constructor(key, code, message) {
      if (key !== trackKey) throw new TypeError('Illegal constructor');
      Object.defineProperties(this, {
        code: { value: code, enumerable: true },
        message: { value: message, enumerable: true },
      });
    }
  }
  globalThis.MediaError = MediaError;

  function videoState(video) {
    let state = videoStates.get(video);
    if (!state) {
      state = { source: '', epoch: 0, ready: 0, network: 0, paused: true, ended: false,
        time: 0, anchor: 0, rate: 1, duration: NaN, width: 0, height: 0,
        frames: [], frame: -1, error: null, timer: null, load: null,
        volume: 1, muted: video.hasAttribute('muted'), seeking: false, queued: false,
        needsLoad: false, decodedBytes: 0 };
      videoStates.set(video, state);
      video.addEventListener('keydown', event => {
        if (!event.isTrusted || event.defaultPrevented || event.key !== ' ' || !video.controls || document.activeElement !== video) return;
        event.preventDefault();
        if (video.paused) video.play().then(undefined, () => {});
        else video.pause();
      });
    }
    return state;
  }
  function selectedSource(video) {
    let source = video.getAttribute('src') || '';
    if (!source) {
      const child = Array.from(video.querySelectorAll('source')).find(element =>
        !element.getAttribute('type') || supportedType(element.getAttribute('type')));
      source = child?.getAttribute('src') || '';
    }
    return source ? new URL(source, video.baseURI || location.href).href : '';
  }
  function clearSurface(video, state) {
    state.frame = -1;
    delete video._ctx;
    __obscuraCore.ops.op_media_register_surface(video._nid >>> 0, 0, 0, new Uint8Array(), _realmFrameId);
  }
  function paintFrame(video, state) {
    if (!state.frames.length) return;
    let index = 0;
    while (index + 1 < state.frames.length && state.frames[index + 1].time <= state.time * 1000) index++;
    if (index === state.frame) return;
    const bytes = state.frames[index].pixels;
    if (!__obscuraCore.ops.op_media_register_surface(video._nid >>> 0, state.width, state.height, bytes, _realmFrameId)) {
      throw failure('The video frame exceeds the surface limit.', 'QuotaExceededError');
    }
    video._ctx = { _w: state.width, _h: state.height, _buf: bytes };
    state.frame = index;
  }
  function synchronize(video, state) {
    if (!state.paused && state.ready >= 2) {
      const elapsed = Math.max(0, now() - state.anchor) * state.rate / 1000;
      state.time += elapsed;
      state.anchor = now();
      if (state.time >= state.duration) {
        if (video.loop && state.duration > 0) state.time %= state.duration;
        else {
          state.time = state.duration;
          state.paused = true; state.ended = true; clearTimeout(state.timer);
          queue(() => { emit(video, 'timeupdate'); emit(video, 'pause'); emit(video, 'ended'); });
        }
      }
    }
    paintFrame(video, state);
  }
  function playbackTick(video, state) {
    if (state.paused) return;
    synchronize(video, state);
    if (!state.paused) {
      emit(video, 'timeupdate');
      state.timer = setTimeout(() => playbackTick(video, state), 40);
    }
  }
  function mediaFailure(video, state, epoch, error) {
    if (epoch !== state.epoch) return;
    state.ready = 0; state.network = 3; state.paused = true;
    state.error = new MediaError(trackKey, 4, String(error.message || error));
    decodedBytes -= state.decodedBytes; state.decodedBytes = 0;
    state.frames = []; state.width = 0; state.height = 0; state.duration = NaN;
    clearSurface(video, state);
    queue(() => { if (epoch === state.epoch) emit(video, 'error'); });
  }
  async function sourceBytes(source, signal) {
    const blob = blobUrls.get(source);
    if (blob) {
      if (blob.size > MAX_BYTES) throw failure('The media resource exceeds the byte limit.', 'QuotaExceededError');
      return new Uint8Array(await blob.arrayBuffer());
    }
    if (source.startsWith('blob:')) throw failure('The media object URL is unavailable.');
    const response = await fetch(source, { signal });
    if (!response.ok) throw failure('The media resource request failed.');
    if (Number(response.headers.get('content-length')) > MAX_BYTES) throw failure('The media resource exceeds the byte limit.', 'QuotaExceededError');
    const bytes = new Uint8Array(await response.arrayBuffer());
    if (bytes.length > MAX_BYTES) throw failure('The media resource exceeds the byte limit.', 'QuotaExceededError');
    return bytes;
  }
  function loadVideo(video, force = false) {
    const state = videoState(video);
    const source = selectedSource(video);
    if (!force && !state.needsLoad && source === state.source && state.load) return state.load;
    state.needsLoad = false;
    state.abort?.abort();
    clearTimeout(state.timer);
    state.source = source; state.epoch++;
    state.ready = 0; state.network = source ? 2 : 0; state.error = null;
    state.paused = true; state.ended = false; state.time = 0;
    decodedBytes -= state.decodedBytes; state.decodedBytes = 0;
    state.duration = NaN; state.width = 0; state.height = 0; state.frames = [];
    clearSurface(video, state);
    const epoch = state.epoch;
    if (!source) { state.load = Promise.resolve(); return state.load; }
    state.abort = new AbortController();
    queue(() => { if (epoch === state.epoch) emit(video, 'loadstart'); });
    state.load = sourceBytes(source, state.abort.signal)
      .then(bytes => __obscuraCore.ops.op_media_decode_webm(bytes))
      .then(packed => {
        if (epoch !== state.epoch) throw failure('The media source changed.', 'AbortError');
        const view = new DataView(packed.buffer, packed.byteOffset, packed.byteLength);
        if (packed.byteLength < 24) throw failure('The decoded media header is invalid.');
        const width = view.getUint32(0, true), height = view.getUint32(4, true);
        const count = view.getUint32(8, true), duration = view.getFloat64(16, true);
        const frameBytes = width * height * 4;
        if (!width || !height || width > 4096 || height > 4096 || width * height > 4 * 1024 * 1024
            || !count || count > MAX_FRAMES || view.getUint32(12, true) !== 0
            || !Number.isFinite(duration) || duration <= 0 || duration > MAX_DURATION
            || packed.byteLength !== 24 + count * (8 + frameBytes) || packed.byteLength > MAX_BYTES) {
          throw failure('The decoded media payload is invalid.');
        }
        if (packed.byteLength + decodedBytes > MAX_BYTES) throw failure('The page exceeds the decoded media byte limit.', 'QuotaExceededError');
        const frames = [];
        let offset = 24, previous = -1;
        for (let index = 0; index < count; index++) {
          const time = view.getFloat64(offset, true); offset += 8;
          if (!Number.isFinite(time) || time < 0 || time < previous || time > duration) throw failure('The decoded media timestamp is invalid.');
          frames.push({ time, pixels: packed.subarray(offset, offset + frameBytes) });
          offset += frameBytes; previous = time;
        }
        state.width = width; state.height = height; state.duration = duration / 1000;
        state.decodedBytes = packed.byteLength; decodedBytes += packed.byteLength;
        state.frames = frames; state.ready = 4; state.network = 1;
        paintFrame(video, state);
        return new Promise((resolve, reject) => queue(() => {
          if (epoch !== state.epoch) { reject(failure('The media source changed.', 'AbortError')); return; }
          emit(video, 'durationchange'); emit(video, 'loadedmetadata');
          emit(video, 'loadeddata'); emit(video, 'canplay'); emit(video, 'canplaythrough');
          resolve();
          if (video.autoplay) video.play().then(undefined, () => {});
        }));
      });
    state.load.then(undefined, error => mediaFailure(video, state, epoch, error));
    return state.load;
  }

  HTMLMediaElement.prototype.canPlayType = function(type) {
    return String(type).trim() && supportedType(type) ? 'probably' : '';
  };
  HTMLMediaElement.prototype.load = function() { loadVideo(this, true).then(undefined, () => {}); };
  HTMLMediaElement.prototype.play = function() {
    const video = this;
    const state = videoState(video);
    return loadVideo(video).then(() => {
      if (!state.frames.length || state.error) throw failure('The element has no supported source.');
      if (!state.paused) return;
      if (state.ended) { state.time = 0; state.ended = false; }
      state.paused = false; state.anchor = now();
      const epoch = state.epoch;
      return new Promise((resolve, reject) => queue(() => {
        if (state.paused || epoch !== state.epoch) { reject(failure('Playback was interrupted.', 'AbortError')); return; }
        emit(video, 'play'); emit(video, 'playing');
        playbackTick(video, state); resolve();
      }));
    }, error => Promise.reject(error?.name === 'AbortError' ? error : failure('The element has no supported source.')));
  };
  HTMLMediaElement.prototype.pause = function() {
    const state = videoState(this);
    synchronize(this, state);
    if (state.paused) return;
    state.paused = true; clearTimeout(state.timer);
    queue(() => { emit(this, 'timeupdate'); emit(this, 'pause'); });
  };
  const mediaProperties = {
    paused: { get() { return videoState(this).paused; } },
    ended: { get() { const state = videoState(this); synchronize(this, state); return state.ended; } },
    readyState: { get() { return videoState(this).ready; } },
    networkState: { get() { return videoState(this).network; } },
    error: { get() { return videoState(this).error; } },
    seeking: { get() { return videoState(this).seeking; } },
    currentSrc: { get() { return videoState(this).source; } },
    duration: { get() { return videoState(this).duration; } },
    currentTime: {
      get() { const state = videoState(this); synchronize(this, state); return state.time; },
      set(value) {
        value = Number(value);
        if (!Number.isFinite(value)) throw new TypeError('Current time must be finite');
        const state = videoState(this);
        state.time = Math.max(0, Number.isFinite(state.duration) ? Math.min(value, state.duration) : value);
        state.anchor = now(); state.ended = false; state.seeking = true;
        paintFrame(this, state);
        queue(() => { emit(this, 'seeking'); state.seeking = false; emit(this, 'timeupdate'); emit(this, 'seeked'); });
      },
    },
    playbackRate: {
      get() { return videoState(this).rate; },
      set(value) {
        value = Number(value);
        if (!Number.isFinite(value) || value <= 0) throw failure('Playback rate must be finite and positive.');
        const state = videoState(this); synchronize(this, state); state.rate = value;
        queue(() => emit(this, 'ratechange'));
      },
    },
    volume: {
      get() { return videoState(this).volume; },
      set(value) {
        value = Number(value);
        if (!Number.isFinite(value) || value < 0 || value > 1) throw failure('Volume is outside 0 to 1.', 'IndexSizeError');
        videoState(this).volume = value; queue(() => emit(this, 'volumechange'));
      },
    },
    muted: {
      get() { return videoState(this).muted; },
      set(value) { videoState(this).muted = !!value; queue(() => emit(this, 'volumechange')); },
    },
  };
  for (const [name, descriptor] of Object.entries(mediaProperties)) {
    Object.defineProperty(HTMLMediaElement.prototype, name, { ...descriptor, enumerable: true, configurable: true });
  }
  for (const name of ['controls', 'autoplay', 'loop', 'defaultMuted']) {
    const attribute = name === 'defaultMuted' ? 'muted' : name;
    Object.defineProperty(HTMLMediaElement.prototype, name, { configurable: true, enumerable: true,
      get() { return this.hasAttribute(attribute); },
      set(value) { if (value) this.setAttribute(attribute, ''); else this.removeAttribute(attribute); },
    });
  }
  Object.defineProperties(HTMLVideoElement.prototype, {
    videoWidth: { configurable: true, enumerable: true, get() { return videoState(this).width; } },
    videoHeight: { configurable: true, enumerable: true, get() { return videoState(this).height; } },
  });
  for (const name of ['width', 'height']) {
    Object.defineProperty(HTMLVideoElement.prototype, name, { configurable: true, enumerable: true,
      get() { return Math.max(0, Number.parseInt(this.getAttribute(name), 10) || 0); },
      set(value) { this.setAttribute(name, Number(value) >>> 0); },
    });
  }

  _mediaAttributeChanged = (element, name) => {
    if ((element.localName === 'video' || element.localName === 'audio') && (name === 'src' || name === 'children')) {
      const state = videoState(element);
      if (name === 'src') state.needsLoad = true;
      if (!state.queued) {
        state.queued = true;
        queue(() => { state.queued = false; loadVideo(element).then(undefined, () => {}); });
      }
    } else if (element.localName === 'source' && (name === 'src' || name === 'type')) {
      const parent = element.parentElement;
      if (parent && (parent.localName === 'video' || parent.localName === 'audio')) _mediaAttributeChanged(parent, 'children');
    }
  };
  _mediaTreeChanged = root => {
    if (root.nodeType === 1 && (root.localName === 'video' || root.localName === 'audio')) _mediaAttributeChanged(root, 'children');
    // Internal traversal must not resolve a public NodeList's realm: an iframe
    // shim can be under construction and its defaultView would re-enter it.
    const ids = _domParse('query_selector_all_scoped', root._nid, 'video,audio') || [];
    for (const nid of ids) _mediaAttributeChanged(_wrapEl(nid), 'children');
    const parent = root.parentElement;
    if (parent && (parent.localName === 'video' || parent.localName === 'audio')) _mediaAttributeChanged(parent, 'children');
  };
  _mediaInitializeDocument = document => _mediaTreeChanged(document);

  for (const constructor of [MediaStream, MediaStreamTrack, CanvasCaptureMediaStreamTrack, MediaRecorder, BlobEvent, MediaError]) {
    _markNative(constructor);
    for (const name of Object.getOwnPropertyNames(constructor.prototype)) {
      const descriptor = Object.getOwnPropertyDescriptor(constructor.prototype, name);
      if (typeof descriptor.value === 'function') _markNative(descriptor.value);
      if (descriptor.get) _markNative(descriptor.get);
      if (descriptor.set) _markNative(descriptor.set);
    }
  }
  for (const name of ['captureStream']) _markNative(HTMLCanvasElement.prototype[name]);
  for (const name of ['canPlayType', 'load', 'play', 'pause']) _markNative(HTMLMediaElement.prototype[name]);
  _markNative(URL.createObjectURL); _markNative(URL.revokeObjectURL);
})();
