use glow::HasContext;
use tessera_core::MediaItem;
use tessera_mpv::{Mpv, MpvEvent, RenderContext};

pub struct TileOptions {
    pub hwdec: String,
    pub audio: bool,
    pub loop_file: bool,
    pub log_verbose: bool,
}

pub enum TileEvent {
    EndOfFile,
    Loaded { width: u32, height: u32, fps: f64, codec: String },
}

pub struct Tile {
    pub render: RenderContext,
    pub mpv: Mpv,
    pub item: MediaItem,
    pub id: usize,
    texture: Option<<glow::Context as HasContext>::Texture>,
    fbo: Option<<glow::Context as HasContext>::Framebuffer>,
    tex_w: i32,
    tex_h: i32,
    pub frames: u64,
    pub last_time_pos: f64,
    pub min_rate: f64,
    pub loaded: bool,
}

impl Tile {
    pub fn new(
        lib: &std::sync::Arc<tessera_mpv::MpvLib>,
        id: usize,
        item: &MediaItem,
        opts: &TileOptions,
    ) -> Result<Tile, tessera_mpv::Error> {
        let mut options: Vec<(&str, &str)> = vec![
            ("vo", "libmpv"),
            ("hwdec", opts.hwdec.as_str()),
            ("idle", "yes"),
            ("osc", "no"),
            ("terminal", "no"),
            ("keep-open", "no"),
        ];
        if opts.audio {
            options.push(("ao", "pipewire,alsa,null"));
        } else {
            options.push(("audio", "no"));
        }
        if opts.loop_file {
            options.push(("loop-file", "inf"));
        }
        options.push(("msg-level", if opts.log_verbose { "all=v" } else { "all=error" }));

        let mpv = Mpv::new(lib, &options)?;
        let render = RenderContext::new_gl(&mpv, crate::compositor::mpv_get_proc_address, std::ptr::null_mut())?;
        mpv.load_file(&item.uri, "replace")?;

        Ok(Tile {
            render,
            mpv,
            item: item.clone(),
            id,
            texture: None,
            fbo: None,
            tex_w: 0,
            tex_h: 0,
            frames: 0,
            last_time_pos: -1.0,
            min_rate: f64::MAX,
            loaded: false,
        })
    }

    pub fn load(&self, item: &MediaItem) -> Result<(), tessera_mpv::Error> {
        self.mpv.load_file(&item.uri, "replace")
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.mpv.command(&["set_property", "pause", if paused { "yes" } else { "no" }]);
    }

    pub fn is_paused(&self) -> bool {
        self.mpv.get_string("pause").map(|v| v == "yes").unwrap_or(false)
    }

    /// Polls mpv events and renders any new frame into this tile's FBO texture.
    /// Must be called with the tile's screen GL context current.
    pub fn tick(&mut self, gl: &glow::Context) -> Vec<TileEvent> {
        let mut events = Vec::new();
        while let Some(ev) = self.mpv.poll_event() {
            match ev {
                MpvEvent::FileLoaded => {
                    let w = self.mpv.get_i64("width").unwrap_or(0) as u32;
                    let h = self.mpv.get_i64("height").unwrap_or(0) as u32;
                    let fps = self
                        .mpv
                        .get_f64("container-fps")
                        .or_else(|| self.mpv.get_f64("estimated-vf-fps"))
                        .unwrap_or(30.0);
                    let codec = self.mpv.get_string("video-codec").unwrap_or_default();
                    self.loaded = true;
                    events.push(TileEvent::Loaded { width: w, height: h, fps, codec });
                }
                MpvEvent::EndFile { reason: tessera_mpv::EndFileReason::Eof, .. } => {
                    events.push(TileEvent::EndOfFile);
                }
                MpvEvent::EndFile { reason, error } => {
                    if error != 0 {
                        eprintln!("tile {}: stream error ({reason:?}, code {error})", self.id);
                    }
                }
                MpvEvent::Shutdown => {
                    eprintln!("tile {}: mpv shutdown", self.id);
                }
                _ => {}
            }
        }

        let flags = self.render.update();
        if flags & tessera_mpv::MPV_RENDER_UPDATE_FRAME != 0 {
            self.ensure_texture(gl);
            if let Some(fbo) = self.fbo {
                unsafe {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                    gl.viewport(0, 0, self.tex_w, self.tex_h);
                }
                if let Err(e) = self.render.render(0, self.tex_w, self.tex_h, true) {
                    eprintln!("tile {}: render failed: {e}", self.id);
                }
                unsafe {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                }
                self.frames += 1;
            }
        }

        let tp = self.mpv.get_f64("time-pos").unwrap_or(-1.0);
        if tp >= 0.0 && self.last_time_pos >= 0.0 {
            let delta = tp - self.last_time_pos;
            if delta >= 0.0 && delta < 10.0 {
                self.min_rate = self.min_rate.min(delta);
            }
        }
        if tp >= 0.0 {
            self.last_time_pos = tp;
        }

        events
    }

    fn ensure_texture(&mut self, gl: &glow::Context) {
        let w = self.mpv.get_i64("width").unwrap_or(0) as i32;
        let h = self.mpv.get_i64("height").unwrap_or(0) as i32;
        if w <= 0 || h <= 0 {
            return;
        }
        if self.texture.is_some() && w == self.tex_w && h == self.tex_h {
            return;
        }
        unsafe {
            if let Some(t) = self.texture.take() {
                gl.delete_texture(t);
            }
            if let Some(f) = self.fbo.take() {
                gl.delete_framebuffer(f);
            }
            let tex = gl.create_texture().expect("tile texture");
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGB8 as i32, w, h, 0, glow::RGB, glow::UNSIGNED_BYTE, None);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

            let fbo = gl.create_framebuffer().expect("tile fbo");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);

            self.texture = Some(tex);
            self.fbo = Some(fbo);
            self.tex_w = w;
            self.tex_h = h;
        }
    }

    pub fn texture(&self) -> Option<<glow::Context as HasContext>::Texture> {
        self.texture
    }

    pub fn video_aspect(&self) -> f32 {
        if self.tex_w > 0 && self.tex_h > 0 {
            self.tex_w as f32 / self.tex_h as f32
        } else {
            16.0 / 9.0
        }
    }

    pub fn status(&self) -> String {
        let hwdec = self.mpv.get_string("hwdec-current").unwrap_or_else(|| "n/a".into());
        let drops = self.mpv.get_i64("decoder-frame-drop-count").unwrap_or(-1);
        let paused = self.is_paused();
        format!(
            "{} [{}x{}] {} drops={} {}",
            self.item.title,
            self.tex_w,
            self.tex_h,
            hwdec,
            drops,
            if paused { "paused" } else { "playing" }
        )
    }

    /// Frees GL resources. Must be called with the tile's screen GL context current.
    pub fn destroy_gl(&mut self, gl: &glow::Context) {
        unsafe {
            if let Some(t) = self.texture.take() {
                gl.delete_texture(t);
            }
            if let Some(f) = self.fbo.take() {
                gl.delete_framebuffer(f);
            }
        }
    }
}
