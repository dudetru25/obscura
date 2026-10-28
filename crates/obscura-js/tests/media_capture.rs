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
