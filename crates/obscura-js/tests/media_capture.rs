#![cfg(feature = "media")]

use obscura_dom::parse_html;
use obscura_js::runtime::ObscuraJsRuntime;
use serde_json::json;

fn runtime() -> ObscuraJsRuntime {
    let mut runtime = ObscuraJsRuntime::new();
    runtime.set_dom(parse_html("<html><body style='margin:0'></body></html>"));
    runtime.set_url("https://media.example.invalid/");
    runtime.set_viewport(100.0, 100.0);
    runtime.run_page_init();
    runtime
}

fn runtime_with_video_fixture() -> ObscuraJsRuntime {
    let mut runtime = runtime();
    runtime.evaluate_script(r#"(() => {
        globalThis.createVideoFixture = async (width = 4, height = 4, frameCount = 2) => {
            let clock = 0;
            Object.defineProperty(performance, 'now', {configurable:true, value:() => clock});
            const canvas = document.createElement('canvas');
            canvas.width = width; canvas.height = height;
            const context = canvas.getContext('2d');
            const stream = canvas.captureStream(0), track = stream.getVideoTracks()[0];
            const recorder = new MediaRecorder(stream), chunks = [];
            recorder.start();
            for (let index = 0; index < frameCount; index++) {
                clock = index * 40;
                context.fillStyle = index === frameCount - 1 ? '#0000ff' : '#ff0000';
                context.fillRect(0,0,width,height);
                const flushed = new Promise((resolve,reject) => {
                    recorder.ondataavailable = event => {
                        if (event.data.size) chunks.push(event.data);
                        resolve();
                    };
                    recorder.onerror = event => reject(event.error);
                });
                track.requestFrame(); recorder.requestData();
                await flushed;
            }
            const stopped = new Promise((resolve,reject) => {
                recorder.onstop = resolve; recorder.onerror = event => reject(event.error);
            });
            recorder.stop(); track.stop(); await stopped;
            return {blob:new Blob(chunks, {type:'video/webm'}), advance:ms => { clock += ms; }};
        };
        globalThis.loadVideoFixture = async (video, blob) => {
            const loaded = new Promise((resolve,reject) => {
                video.onloadeddata = resolve;
                video.onerror = () => reject(new Error(video.error.message));
            });
            const url = URL.createObjectURL(blob);
            video.src = url;
            await loaded;
            return url;
        };
        globalThis.nextMediaTask = () => new Promise(resolve => setTimeout(resolve, 0));
    })()"#).expect("media fixture helpers must initialize");
    runtime
}

#[tokio::test]
async fn preload_none_defers_loading_until_play_is_requested() {
    let mut runtime = runtime_with_video_fixture();
    let result = runtime.evaluate_for_cdp(r#"(async () => {
        const fixture = await createVideoFixture();
        const video = document.createElement('video');
        video.setAttribute('preload', 'none');
        const events = [];
        for (const type of ['loadedmetadata','loadeddata','playing']) {
            video.addEventListener(type, () => events.push(type));
        }
        const url = URL.createObjectURL(fixture.blob);
        video.src = url;
        document.body.append(video);
        await nextMediaTask(); await nextMediaTask();
        const deferred = [video.readyState,video.networkState,video.error,events.slice()];
        await video.play();
        const playing = [video.readyState >= video.HAVE_CURRENT_DATA,video.paused,
            video.videoWidth,video.videoHeight,events.includes('loadeddata'),events.includes('playing')];
        video.pause(); URL.revokeObjectURL(url);
        return {deferred,playing};
    })()"#, true, true).await.expect("deferred media must start on play");
    assert_eq!(result.value.unwrap(), json!({
        "deferred":[0,1,null,[]],
        "playing":[true,false,4,4,true,true],
    }));
}

#[tokio::test]
async fn removing_a_video_pauses_only_if_it_stays_detached() {
    let mut runtime = runtime_with_video_fixture();
    let result = runtime.evaluate_for_cdp(r#"(async () => {
        const fixture = await createVideoFixture(4,4,4);
        const video = document.createElement('video');
        document.body.append(video);
        const url = await loadVideoFixture(video, fixture.blob);
        let pauses = 0;
        video.addEventListener('pause', () => pauses++);
        await video.play();
        video.remove(); document.body.append(video);
        await nextMediaTask();
        fixture.advance(20);
        const reinserted = [video.paused,video.currentTime > 0,pauses];
        const beforeRemoval = video.currentTime;
        video.remove();
        await nextMediaTask(); await nextMediaTask();
        fixture.advance(20);
        const detached = [video.paused,video.currentTime === beforeRemoval,pauses];
        document.body.append(video);
        await nextMediaTask();
        const preserved = [video.paused,video.currentTime === beforeRemoval];
        await video.play(); fixture.advance(20);
        const resumed = [video.paused,video.currentTime > beforeRemoval];
        video.pause(); URL.revokeObjectURL(url);
        return {reinserted,detached,preserved,resumed};
    })()"#, true, true).await.expect("video removal and reinsertion must settle");
    assert_eq!(result.value.unwrap(), json!({
        "reinserted":[false,true,0],
        "detached":[true,true,1],
        "preserved":[true,true],
        "resumed":[false,true],
    }));
}

#[tokio::test]
async fn full_hd_video_plays_beyond_the_whole_clip_decoded_buffer_limit() {
    let mut runtime = runtime_with_video_fixture();
    let result = runtime.evaluate_for_cdp_with_timeout(r#"(async () => {
        const fixture = await createVideoFixture(1920,1080,17);
        const video = document.createElement('video');
        document.body.append(video);
        const url = await loadVideoFixture(video, fixture.blob);
        await video.play(); video.pause();
        const sought = new Promise(resolve => video.addEventListener('seeked', resolve, {once:true}));
        video.currentTime = .64;
        await sought;
        const canvas = document.createElement('canvas'); canvas.width = 1; canvas.height = 1;
        const context = canvas.getContext('2d'); context.drawImage(video,0,0,1,1);
        const pixel = Array.from(context.getImageData(0,0,1,1).data);
        const result = {dimensions:[video.videoWidth,video.videoHeight],
            time:video.currentTime,pixel,error:video.error};
        URL.revokeObjectURL(url);
        return result;
    })()"#, true, true, 30_000).await.expect("full-HD playback must decode beyond sixteen frames");
    assert!(!result.thrown, "full-HD playback rejected: {}", result.description);
    let value = result.value.expect("full-HD playback result");
    assert_eq!(value["dimensions"], json!([1920,1080]));
    assert_eq!(value["time"], 0.64);
    assert_eq!(value["error"], serde_json::Value::Null);
    assert!(value["pixel"][0].as_u64().unwrap() < 50, "last frame must be blue: {value}");
    assert!(value["pixel"][2].as_u64().unwrap() > 200, "last frame must be blue: {value}");
    assert_eq!(value["pixel"][3], 255);
}

#[tokio::test]
async fn canvas_recording_chunks_decode_and_play_with_real_frame_pixels() {
    let mut runtime = runtime();
    let result = runtime.evaluate_for_cdp(r#"(async () => {
        let clock = 0;
        Object.defineProperty(performance, 'now', {configurable:true, value:() => clock});
        const canvas = document.createElement('canvas');
        canvas.width = 4; canvas.height = 4;
        const context = canvas.getContext('2d');
        const stream = canvas.captureStream(0), track = stream.getVideoTracks()[0];
        const recorder = new MediaRecorder(stream, {mimeType:'video/webm;codecs=vp8'});
        const events = [], chunks = [];
        for (const type of ['start','pause','resume','dataavailable','stop']) {
            recorder.addEventListener(type, () => events.push(type));
        }
        recorder.ondataavailable = event => { if (event.data.size) chunks.push(event.data); };
        const stopped = new Promise((resolve, reject) => {
            recorder.onstop = resolve; recorder.onerror = event => reject(event.error);
        });
        recorder.start(100);
        context.fillStyle = '#ff0000'; context.fillRect(0,0,4,4); track.requestFrame();
        recorder.requestData();
        recorder.pause(); clock = 50;
        context.fillStyle = '#00ff00'; context.fillRect(0,0,4,4); track.requestFrame();
        recorder.resume(); clock = 150;
        context.fillStyle = '#0000ff'; context.fillRect(0,0,4,4); track.requestFrame();
        recorder.stop(); track.stop();
        await stopped;
        const blob = new Blob(chunks, {type:'video/webm'});
        const bytes = new Uint8Array(await blob.arrayBuffer());
        const header = Array.from(bytes.subarray(0,4));
        const hasCluster = bytes.some((value,index) => value === 0x1f && bytes[index+1] === 0x43
            && bytes[index+2] === 0xb6 && bytes[index+3] === 0x75);
        const video = document.createElement('video');
        const mediaEvents = [];
        for (const type of ['loadedmetadata','loadeddata','playing','pause','seeked','ended']) {
            video.addEventListener(type, () => mediaEvents.push(type));
        }
        document.body.append(video);
        const loaded = new Promise((resolve,reject) => {
            video.onloadeddata = resolve; video.onerror = () => reject(new Error('Video decode failed'));
        });
        const url = URL.createObjectURL(blob);
        video.src = url;
        await loaded;
        const sample = () => {
            const output = document.createElement('canvas'); output.width = 4; output.height = 4;
            const drawing = output.getContext('2d'); drawing.drawImage(video,0,0);
            return Array.from(drawing.getImageData(0,0,1,1).data);
        };
        const first = sample();
        const bounds = video.getBoundingClientRect();
        await video.play(); clock += 30;
        const advanced = video.currentTime > 0 && !video.paused;
        video.pause();
        const paused = video.paused;
        video.currentTime = .1;
        const second = sample();
        video.currentTime = 0;
        const reset = sample();
        await video.play(); clock += 1000;
        const ended = video.ended && video.paused;
        URL.revokeObjectURL(url);
        return {events, chunks:chunks.length, header, hasCluster, mime:blob.type,
            dimensions:[video.videoWidth,video.videoHeight,bounds.width,bounds.height],
            first,second,reset,advanced,paused,ended,mediaEvents,
            lifecycle:[recorder.state,track.readyState,stream.active],
            support:[MediaRecorder.isTypeSupported('video/webm;codecs=vp8'),
                MediaRecorder.isTypeSupported('video/mp4'), video.canPlayType('video/webm; codecs="vp8"')]};
    })()"#, true, true).await.expect("recording and playback must complete");
    let value = result.value.expect("recording result");
    assert_eq!(value["events"], json!(["start","dataavailable","pause","resume","dataavailable","stop"]));
    assert_eq!(value["chunks"], 2);
    assert_eq!(value["header"], json!([0x1a,0x45,0xdf,0xa3]));
    assert_eq!(value["hasCluster"], true);
    assert_eq!(value["mime"], "video/webm");
    assert_eq!(value["dimensions"], json!([4,4,4,4]));
    for sample in ["first", "reset"] {
        assert!(value[sample][0].as_u64().unwrap() > 200, "red decoded frame: {value}");
        assert!(value[sample][2].as_u64().unwrap() < 50, "red decoded frame: {value}");
    }
    assert!(value["second"][2].as_u64().unwrap() > 200, "blue decoded frame: {value}");
    assert!(value["second"][0].as_u64().unwrap() < 50, "blue decoded frame: {value}");
    assert_eq!(value["advanced"], true);
    assert_eq!(value["paused"], true);
    assert_eq!(value["ended"], true);
    assert_eq!(value["lifecycle"], json!(["inactive","ended",false]));
    assert_eq!(value["support"], json!([true,false,"probably"]));
    assert_eq!(value["mediaEvents"][0], "loadedmetadata");
    assert_eq!(value["mediaEvents"][1], "loadeddata");
}

#[tokio::test]
async fn invalid_media_fails_explicitly_and_source_reset_discards_old_frames() {
    let mut runtime = runtime();
    for expression in [
        "document.createElement('canvas').captureStream(-1)",
        "document.createElement('canvas').captureStream(Infinity)",
        "new MediaRecorder(new MediaStream(), {mimeType:'video/mp4'})",
        "new MediaRecorder(new MediaStream()).start()",
        "new MediaRecorder(new MediaStream()).requestData()",
        "document.createElement('video').currentTime = NaN",
        "document.createElement('video').volume = 2",
    ] {
        assert!(runtime.evaluate_script(expression).is_err(), "must reject {expression}");
    }
    let result = runtime.evaluate_for_cdp(r#"(async () => {
        const canvas = document.createElement('canvas'); canvas.width = 4; canvas.height = 4;
        const context = canvas.getContext('2d');
        const stream = canvas.captureStream(0), track = stream.getVideoTracks()[0];
        const recorder = new MediaRecorder(stream), chunks = [];
        recorder.ondataavailable = event => chunks.push(event.data);
        const stopped = new Promise((resolve,reject) => {
            recorder.onstop = resolve; recorder.onerror = event => reject(event.error);
        });
        recorder.start(); context.fillStyle = '#ff0000'; context.fillRect(0,0,4,4); track.requestFrame();
        recorder.stop(); track.stop(); await stopped;
        const video = document.createElement('video'); document.body.append(video);
        const valid = URL.createObjectURL(new Blob(chunks, {type:'video/webm'}));
        const loaded = new Promise((resolve,reject) => {
            video.onloadeddata = resolve; video.onerror = reject;
        });
        video.src = valid; await loaded;
        const before = [video.readyState >= 2, video.videoWidth, video.videoHeight];
        const invalid = URL.createObjectURL(new Blob([new Uint8Array([1,2,3])], {type:'video/webm'}));
        const errored = new Promise(resolve => { video.onerror = resolve; });
        video.src = invalid; await errored;
        const playError = await video.play().then(() => null, error => error.name);
        const after = [video.readyState, video.videoWidth, video.videoHeight, video.error.code, playError];
        video.removeAttribute('src'); video.load();
        const cleared = [video.readyState,video.networkState,video.error,video.currentTime];
        URL.revokeObjectURL(valid); URL.revokeObjectURL(invalid);
        return {before,after,cleared};
    })()"#, true, true).await.expect("error lifecycle must settle");
    assert_eq!(result.value.unwrap(), json!({
        "before":[true,4,4],
        "after":[0,0,0,4,"NotSupportedError"],
        "cleared":[0,0,null,0],
    }));
}

#[tokio::test]
async fn stopping_a_capture_track_finishes_its_recorder_and_preserves_clones() {
    let mut runtime = runtime();
    let result = runtime.evaluate_for_cdp(r#"(async () => {
        const canvas = document.createElement('canvas'); canvas.width = 4; canvas.height = 4;
        const context = canvas.getContext('2d');
        const stream = canvas.captureStream(20), track = stream.getVideoTracks()[0];
        const copy = stream.clone(), clonedTrack = copy.getVideoTracks()[0];
        const recorder = new MediaRecorder(stream);
        const chunks = [];
        recorder.ondataavailable = event => chunks.push(event.data.size);
        const stopped = new Promise((resolve,reject) => {
            recorder.onstop = resolve; recorder.onerror = event => reject(event.error);
        });
        recorder.start(); context.fillStyle = '#668cff'; context.fillRect(0,0,4,4); track.requestFrame();
        track.stop(); await stopped;
        const result = [recorder.state,track.readyState,clonedTrack.readyState,stream.active,copy.active,
            track !== clonedTrack,track.id !== clonedTrack.id,clonedTrack.canvas === canvas,
            clonedTrack.getSettings().frameRate,chunks.some(size => size > 0)];
        clonedTrack.stop();
        return result;
    })()"#, true, true).await.expect("track termination must finish recording");
    assert_eq!(result.value.unwrap(), json!(["inactive","ended","live",false,true,true,true,true,20,true]));
}
