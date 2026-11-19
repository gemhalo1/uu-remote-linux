//! H.264/H.265 decoding through an external `ffmpeg` process, like the host's encoder pipeline.
//! Needs only the ffmpeg CLI, not FFmpeg development libraries.
//! Frames are decoded at the host's native size (resampling the picture blurs text); ffmpeg's verbose
//! scaler line gives the size before the first frame. `UU_VIEWER_SIZE` keeps the old fixed letterboxed
//! canvas, where the stream line tells the window which sub-rectangle is picture content.
use super::desktop::{Decoder,Frame};
use super::media::Codec;
use std::io::{BufRead,Read,Write};
use std::process::{Child,Command,Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool,AtomicU64,Ordering};
use std::sync::mpsc::{Receiver,TryRecvError};

pub struct Cli { child:Child, input:Option<std::sync::mpsc::SyncSender<Vec<u8>>>, codec:Codec, frames:Receiver<(u32,u32,Vec<u8>)>, fixed:Option<(u32,u32)>, source:Arc<AtomicU64>, resized:Arc<AtomicBool>, thin:bool, need_key:bool }

/// Decoder threads: 0 lets ffmpeg use one thread per core (frame threading, which holds back
/// `threads-1` pictures) instead of the single thread that capped 4K H.265 at ~21 fps.
/// `UU_VIEWER_DECODE_THREADS=1` trades that throughput for the lowest latency.
fn decode_threads()->String {
    std::env::var("UU_VIEWER_DECODE_THREADS").ok()
        .and_then(|v|v.trim().parse::<u32>().ok()).filter(|n|*n<=64).unwrap_or(0).to_string()
}

impl Cli {
    /// `UU_VIEWER_SIZE=WxH` scales into a fixed canvas (default: native size); `UU_VIEWER_FFMPEG` the binary.
    pub fn new()->Result<Self,String> {
        let fixed=std::env::var("UU_VIEWER_SIZE").ok()
            .and_then(|v|{let (w,h)=v.split_once('x')?;Some((w.parse().ok()?,h.parse().ok()?))})
            .filter(|&(w,h):&(u32,u32)|(16..=8192).contains(&w)&&(16..=8192).contains(&h)&&w%2==0&&h%2==0);
        Self::with_codec(Codec::H264,fixed)
    }
    fn with_codec(codec:Codec,fixed:Option<(u32,u32)>)->Result<Self,String> {
        let ffmpeg=std::env::var("UU_VIEWER_FFMPEG").unwrap_or_else(|_|"ffmpeg".into());
        let filter=fixed.map(|(width,height)|format!("scale={width}:{height}:force_original_aspect_ratio=decrease:flags=lanczos,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2"));
        let passthrough=passthrough_args(&ffmpeg);
        let mut command=Command::new(&ffmpeg);
        command.args(["-hide_banner","-loglevel","verbose","-nostats","-nostdin","-threads",&decode_threads(),"-probesize","32","-analyzeduration","0",
                "-fflags","nobuffer","-flags","low_delay","-f",codec.demuxer(),"-i","pipe:0","-an"]);
        if let Some(filter)=&filter {command.args(["-vf",filter]);}
        // NV12, not RGB: the renderer converts it on the GPU, so swscale stays out of the per-frame
        // path and half the bytes cross the pipe (measured 4K: upload 25 → 5.8 ms/frame).
        let mut child=command.args(["-pix_fmt","nv12",
                // Raw H.264/H.265 carries no timestamps; constant-rate output dropped almost every frame of
                // the official Mac host's stream (270 → 2). Pass frames through unchanged.
                passthrough.0,passthrough.1,"-f","rawvideo","pipe:1"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().map_err(|e|format!("Start ffmpeg decoder: {e}"))?;
        let mut input=child.stdin.take().ok_or("ffmpeg stdin missing")?;
        let mut output=child.stdout.take().ok_or("ffmpeg stdout missing")?;
        // Source picture size from the one-time stream line; 0 until parsed. Only the numbers are kept.
        let source=Arc::new(AtomicU64::new(0));let resized=Arc::new(AtomicBool::new(false));
        let (size_tx,size_rx)=std::sync::mpsc::sync_channel::<(u32,u32)>(1);
        if let Some((w,h))=fixed {let _=size_tx.send((w,h));}
        if let Some(stderr)=child.stderr.take() {
            let (source,resized)=(source.clone(),resized.clone());
            let _=std::thread::Builder::new().name("uu-viewer-ffmpeg-info".into()).spawn(move||{
                let mut native=None;
                for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                    if fixed.is_some() {
                        if source.load(Ordering::Relaxed)!=0 {continue}
                        if let Some((w,h))=parse_video_size(&line) {source.store((u64::from(w)<<32)|u64::from(h),Ordering::Relaxed);}
                        continue
                    }
                    // Native: the first scaler line fixes the output size; ffmpeg auto-scales a later
                    // resolution change to it, so a different input size means the decoder must restart.
                    let Some((input,out))=parse_scaler_sizes(&line) else {continue};
                    match native {
                        None=>{native=Some(out);let _=size_tx.send(out);},
                        Some(size) if input!=size=>resized.store(true,Ordering::Release),
                        Some(_)=>{},
                    }
                }
            });
        }
        // Feeding the decoder runs on its own thread: a slow decoder must not block the caller's
        // event loop (measured 4K stalls of 75-144 ms per write at 60 fps input).
        let (in_tx,in_rx)=std::sync::mpsc::sync_channel::<Vec<u8>>(2);
        std::thread::Builder::new().name("uu-viewer-ffmpeg-in".into()).spawn(move||{
            for data in in_rx {if input.write_all(&data).is_err() {break}}
        }).map_err(|e|format!("Decoder writer: {e}"))?;
        // Bounded: a slow window drops decoded frames instead of growing memory.
        let (tx,frames)=std::sync::mpsc::sync_channel(2);
        std::thread::Builder::new().name("uu-viewer-decode".into()).spawn(move||{
            let Ok((width,height))=size_rx.recv() else {return};
            let size=width as usize*height as usize*3/2;
            loop {
                let mut frame=vec![0;size];
                if output.read_exact(&mut frame).is_err() {break;}
                match tx.try_send((width,height,frame)) { Ok(())|Err(std::sync::mpsc::TrySendError::Full(_))=>{}, Err(_)=>break }
            }
        }).map_err(|e|format!("Decoder reader: {e}"))?;
        Ok(Self{child,input:Some(in_tx),codec,frames,fixed,source,resized,thin:false,need_key:false})
    }
}

/// Verbose swscale setup line: `[auto_scale_0 @ 0x..] w:3024 h:1964 fmt:yuv420p ... -> w:3024 h:1964 fmt:rgb24 ...`.
fn parse_scaler_sizes(line:&str)->Option<((u32,u32),(u32,u32))> {
    let (input,out)=line.split_once(" -> ")?;
    if !out.contains(" fmt:") {return None}
    fn size(part:&str)->Option<(u32,u32)> {
        let mut words=part.split_whitespace();
        let w=words.find_map(|t|t.strip_prefix("w:"))?.parse().ok()?;
        let h=words.next()?.strip_prefix("h:")?.parse().ok()?;
        ((16..=16384).contains(&w)&&(16..=16384).contains(&h)).then_some((w,h))
    }
    Some((size(input.rsplit_once("] ").map_or(input,|(_,rest)|rest))?,size(out)?))
}

/// `30000/1001`-style or display strings are irrelevant here: the first WxH on a `Video:` line is the
/// coded picture size, which is the aspect the scale+pad filter letterboxes into the canvas.
fn parse_video_size(line:&str)->Option<(u32,u32)> {
    let rest=line.split_once("Video:")?.1;
    rest.split(|c:char|c==','||c==' '||c=='('||c==')'||c=='['||c==']').find_map(|token|{
        let token=token.trim_matches(|c:char|!c.is_ascii_digit()&&c!='x');
        let (w,h)=token.split_once('x')?;
        if w.is_empty()||h.is_empty()||!w.bytes().all(|b|b.is_ascii_digit())||!h.bytes().all(|b|b.is_ascii_digit()) {return None}
        let (w,h)=(w.parse::<u32>().ok()?,h.parse::<u32>().ok()?);
        ((16..=16384).contains(&w)&&(16..=16384).contains(&h)).then_some((w,h))
    })
}

/// FFmpeg ≥ 5.1 has `-fps_mode`; FFmpeg 8+ removed `-vsync`, and Ubuntu 22.04's 4.4 lacks `-fps_mode`.
/// Probe once per binary with a one-frame null run instead of parsing version strings.
fn passthrough_args(ffmpeg:&str)->(&'static str,&'static str) {
    static CACHE:std::sync::Mutex<Vec<(String,bool)>>=std::sync::Mutex::new(Vec::new());
    let mut cache=CACHE.lock().unwrap_or_else(|e|e.into_inner());
    let fps_mode=match cache.iter().find(|(name,_)|name==ffmpeg) {
        Some(&(_,known))=>known,
        None=>{
            let ok=Command::new(ffmpeg).args(["-hide_banner","-loglevel","quiet","-nostdin","-f","lavfi","-i","color=s=16x16:d=0.04",
                "-fps_mode","passthrough","-f","null","-"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
                .status().is_ok_and(|s|s.success());
            cache.push((ffmpeg.into(),ok));ok
        }
    };
    if fps_mode {("-fps_mode","passthrough")} else {("-vsync","0")}
}

impl Decoder for Cli {
    fn set_codec(&mut self,codec:Codec)->Result<(),String>{
        if self.codec!=codec {*self=Self::with_codec(codec,self.fixed)?;}Ok(())
    }
    fn reset(&mut self) {}
    fn decode(&mut self,au:&[u8])->Result<Vec<Frame>,String> {
        if self.resized.load(Ordering::Acquire) {
            // The caller resets and asks for a key frame; the new process sizes itself from it.
            *self=Self::with_codec(self.codec,self.fixed)?;
            return Err("remote resolution changed".into());
        }
        let input=self.input.as_ref().ok_or("ffmpeg input closed")?;
        // ffmpeg consumes its input at decode speed, so at 4K the two-unit queue fills whenever the
        // remote sends faster than this machine decodes. Dropping a unit mid-GOP would smear the
        // pictures that reference it, so skip ahead to the next key frame instead; the decoder keeps
        // its reference pictures and resumes immediately at that key frame (no restart, no stall).
        if self.thin {
            if !super::desktop::is_key_frame(self.codec,au) {return Ok(Vec::new())}
            self.thin=false;
        }
        // The raw-stream parser ends an access unit only when the next one starts; an access unit
        // delimiter right after it releases the picture now instead of one remote frame later.
        let delimiter:&[u8]=match self.codec {Codec::H264=>&[0,0,0,1,0x09,0xf0],Codec::H265=>&[0,0,0,1,0x46,0x01,0x50]};
        let mut unit=Vec::with_capacity(au.len()+delimiter.len());unit.extend_from_slice(au);unit.extend_from_slice(delimiter);
        match input.try_send(unit) {
            Ok(())=>{},
            Err(std::sync::mpsc::TrySendError::Full(_))=>{self.thin=true;self.need_key=true;return Ok(Vec::new())},
            Err(std::sync::mpsc::TrySendError::Disconnected(_))=>return Err("ffmpeg decoder exited".into()),
        }
        self.take_frames()
    }
    fn poll(&mut self)->Vec<Frame> {self.take_frames().unwrap_or_default()}
    fn needs_key_frame(&mut self)->bool {std::mem::take(&mut self.need_key)}
}
impl Cli {
    fn take_frames(&mut self)->Result<Vec<Frame>,String> {
        let mut out=Vec::new();
        loop {
            match self.frames.try_recv() {
                Ok((width,height,data))=>{
                    let source=self.source.load(Ordering::Relaxed);
                    let content=(self.fixed.is_some()&&source!=0).then(||((source>>32) as u32,source as u32));
                    out.push(Frame{width,height,data,pixel:super::desktop::Pixel::Nv12,content})
                },
                Err(TryRecvError::Empty)=>break,
                Err(TryRecvError::Disconnected)=>return Err("ffmpeg decoder exited".into()),
            }
        }
        Ok(out)
    }
}

impl Drop for Cli { fn drop(&mut self) { let _=self.child.kill(); let _=self.child.wait(); } }

#[cfg(test)] mod tests {
    use super::*;
    // Six synthetic red IDRs per fixture; no captured desktop or external encoder dependency.
    #[cfg(unix)] #[test] fn passthrough_follows_ffmpeg_option_support(){
        use std::os::unix::fs::PermissionsExt;
        let dir=std::env::temp_dir().join(format!("uu-ffprobe-{}",std::process::id()));std::fs::create_dir_all(&dir).unwrap();
        for (name,script,expected) in [("new","exit 0",("-fps_mode","passthrough")),("old","case \"$*\" in *-fps_mode*) exit 1;; esac",("-vsync","0"))]{
            let path=dir.join(name);std::fs::write(&path,format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(super::passthrough_args(path.to_str().unwrap()),expected);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test] fn ffmpeg_decodes_h264_and_hevc_420_and_444_to_nv12(){
        match Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status(){
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{eprintln!("skip: ffmpeg is not on PATH");return},
            result=>assert!(result.expect("ffmpeg availability").success()),
        }
        // Native size and the fixed UU_VIEWER_SIZE canvas.
        for fixed in [None,Some((32,32))] {
        let mut cli=Cli::with_codec(Codec::H265,fixed).unwrap();
        for (codec,data) in [
            (Codec::H264,&include_bytes!("../../tests/fixtures/viewer/red-420.h264")[..]),
            (Codec::H265,&include_bytes!("../../tests/fixtures/viewer/red-420.hevc")[..]),
            (Codec::H264,&include_bytes!("../../tests/fixtures/viewer/red-444.h264")[..]),
            (Codec::H265,&include_bytes!("../../tests/fixtures/viewer/red-444.hevc")[..]),
        ] {
            cli.set_codec(codec).unwrap();
            // Exercise the same streaming process and NV12 output as SDL, then drain at EOF.
            let mut frames=cli.decode(data).unwrap();drop(cli.input.take());
            loop {match cli.frames.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok((width,height,data))=>frames.push(Frame{width,height,data,pixel:super::super::desktop::Pixel::Nv12,content:None}),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)=>break,
                Err(e)=>panic!("{codec:?} decoder did not finish: {e}"),
            }}
            let status=cli.child.wait().unwrap();
            assert!(status.success(),"{codec:?}: ffmpeg exited with {status}");
            assert!(!frames.is_empty(),"{codec:?}: no frames");
            for frame in frames {
                assert_eq!(frame.data.len(),32*32*3/2);
                assert_eq!((frame.width,frame.height),(32,32));
                assert!(frame.to_rgb().chunks_exact(3).all(|p|p[0]>200 && p[1]<60 && p[2]<60),"{codec:?}: NV12 picture");
            }
        }
        }
    }
    #[test] fn native_size_is_read_from_the_scaler_line(){
        assert_eq!(super::parse_scaler_sizes("[auto_scale_0 @ 0x7f55ac005f40] w:3024 h:1964 fmt:yuv420p csp:unknown range:unknown sar:1/1 -> w:3024 h:1964 fmt:rgb24 csp:gbr range:pc sar:1/1 flags:0x00000004"),Some(((3024,1964),(3024,1964))));
        assert_eq!(super::parse_scaler_sizes("[auto_scaler_0 @ 0x55d] w:2560 h:1440 fmt:yuv444p sar:1/1 -> w:1920 h:1080 fmt:rgb24 sar:1/1 flags:0x4"),Some(((2560,1440),(1920,1080))));
        assert_eq!(super::parse_scaler_sizes("[Parsed_pad_1 @ 0x7f00] w:32 h:32 -> w:64 h:32 x:16 y:0 color:0x000000FF"),None);
        assert_eq!(super::parse_scaler_sizes("  Stream #0:0 -> #0:0 (h264 (native) -> rawvideo (native))"),None);
    }
    #[test] fn source_size_is_read_from_the_ffmpeg_stream_line(){
        assert_eq!(super::parse_video_size("  Stream #0:0: Video: h264 (High), yuv420p(progressive), 3024x1964, 25 fps"),Some((3024,1964)));
        assert_eq!(super::parse_video_size("Stream #0:0: Video: hevc, yuv420p, 1920x1080 [SAR 1:1 DAR 16:9]"),Some((1920,1080)));
        assert_eq!(super::parse_video_size("Stream #0:0: Audio: opus, 48000 Hz, stereo"),None);
        assert_eq!(super::parse_video_size("frame= 1 fps=0.0 q=0.0"),None);
        assert_eq!(super::parse_video_size("Stream #0:0: Video: h264, 1920xsynth"),None);
    }
}
