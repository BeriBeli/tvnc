//! Command line surface.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, ValueEnum};

/// Bounds for an explicit percentage scale (`--scale 125%`).
pub const MIN_SCALE_PERCENT: u16 = 25;
pub const MAX_SCALE_PERCENT: u16 = 400;

/// Percentage scaling presets offered in the command menu and stepped by `-`/`+`.
pub const SCALE_PRESETS: &[u16] = &[80, 100, 110, 125, 150, 200];

/// A pixel-perfect VNC client for terminals that speak the Kitty graphics protocol.
#[derive(Debug, Parser)]
#[command(name = "tvnc", version, about, long_about = None)]
pub struct Args {
    /// VNC server to connect to, as `host`, `host:port` or `host::port`.
    ///
    /// A bare `host` uses port 5900. A single colon is interpreted the way every
    /// other VNC client does it: `host:1` means display 1, i.e. port 5901. Use a
    /// double colon to give a raw port number: `host::5900`.
    #[arg(value_name = "HOST[:DISPLAY]", required_unless_present_any = ["print_caps", "test_pattern"])]
    pub server: Option<String>,

    /// Read a binary file made by TigerVNC's vncpasswd (also accepts -passwd).
    /// With --view-only, use the view-only password if the file contains one.
    #[arg(long, value_name = "PATH")]
    pub passwd: Option<PathBuf>,

    /// How remote pixels map onto terminal pixels: `native`, `fit`, `integer`,
    /// `1:1`, or a percentage such as `80%`, `110%`, `125%`, `150%`, `200%`.
    #[arg(long, value_name = "MODE|PERCENT%", default_value_t = ScaleMode::Native)]
    pub scale: ScaleMode,

    /// How image data reaches the terminal.
    #[arg(long, value_enum, default_value_t = Transfer::Auto)]
    pub transfer: Transfer,

    /// JPEG quality for Tight, 0 (worst) to 9 (best).
    ///
    /// Unset means no JPEG at all: the protocol says Tight only uses JPEG when a
    /// quality level is given, so leaving this alone is how to ask for a lossless
    /// picture. Set it on a slow link, where lossless is unaffordable.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=9))]
    pub quality: Option<u8>,

    /// Compression level, 0 (least) to 9 (most).
    ///
    /// Lossless either way; this trades the server's CPU against bandwidth. Unset
    /// leaves the choice to the server.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=9))]
    pub compression: Option<u8>,

    /// Target frame rate. Frames are dropped rather than queued when the
    /// terminal cannot keep up.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=240))]
    pub fps: u32,

    /// Never send input to the server.
    #[arg(long)]
    pub view_only: bool,

    /// Draw the remote server's cursor bitmap on top of the desktop.
    ///
    /// Off by default to avoid double cursors: the viewer still negotiates the
    /// `Cursor` pseudo-encoding so the server does not paint a lagging cursor
    /// into the framebuffer, and sets the terminal pointer to the OS arrow.
    #[arg(long)]
    pub remote_cursor: bool,

    /// Do not synchronise clipboards in either direction.
    #[arg(long)]
    pub no_clipboard: bool,

    /// Ask for each frame instead of letting the server push them.
    ///
    /// Pushed frames save a round trip each, and cost the pace: a server pushing as
    /// fast as it can encode decides how hard both ends work, and every update has to
    /// be decoded whether or not there is time to draw it. On a busy screen -- a page
    /// being scrolled -- that can saturate the server, this end, or both. Asking for
    /// one frame at a time gives the pace back, at a round trip per frame.
    #[arg(long)]
    pub no_push: bool,

    /// Key to open the local command menu. Use "none" to open it only by clicking.
    #[arg(long, value_enum, ignore_case = true, default_value_t = MenuKey::F8)]
    pub menu_key: MenuKey,

    /// Reconnect automatically, with backoff, when the session drops.
    #[arg(long)]
    pub reconnect: bool,

    /// Render even if the terminal does not answer the Kitty graphics probe.
    #[arg(long)]
    pub force: bool,

    /// Report terminal capabilities and exit.
    #[arg(long)]
    pub print_caps: bool,

    /// Render a synthetic test pattern instead of connecting to a server.
    #[arg(long)]
    pub test_pattern: bool,

    /// Append diagnostics to this file. Logging is off when unset, because
    /// anything written to stdout would corrupt the graphics stream.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MenuKey {
    F8,
    None,
}

/// How the remote framebuffer is mapped onto the terminal's pixel area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleMode {
    /// Ask the server to resize its desktop to exactly the terminal's pixel
    /// area, so one remote pixel is one terminal pixel. Falls back to `fit` if
    /// the server refuses.
    Native,
    /// Scale to fit, preserving aspect ratio, letterboxing the remainder.
    Fit,
    /// Largest whole-number scale factor that fits. Never interpolates.
    Integer,
    /// No scaling: show a 1:1 window onto the remote desktop and pan it.
    OneToOne,
    /// Scale by a fixed percentage (e.g. `80%`, `100%`, `110%`, `125%`, `150%`, `200%`).
    ///
    /// Like HiDPI display scaling: asks a resize-capable server for
    /// `window / scale` so the scaled desktop fills the window without
    /// cropping. If the server cannot or refuses to resize, zooms the existing
    /// desktop at that percentage and pans when it overflows the window.
    Percent(u16),
}

impl ScaleMode {
    /// The next scaling mode in the cycle, in declaration order and wrapping round.
    pub fn next(self) -> Self {
        match self {
            ScaleMode::Native => ScaleMode::Fit,
            ScaleMode::Fit => ScaleMode::Integer,
            ScaleMode::Integer => ScaleMode::OneToOne,
            ScaleMode::OneToOne | ScaleMode::Percent(_) => ScaleMode::Native,
        }
    }

    /// Whether this mode asks the server to resize its desktop to fit the terminal.
    pub fn negotiates_size(self) -> bool {
        matches!(self, ScaleMode::Native | ScaleMode::Percent(_))
    }

    /// Step up (`delta > 0`) or down (`delta < 0`) through [`SCALE_PRESETS`].
    pub fn step_percent(self, delta: i8) -> Self {
        let current = match self {
            ScaleMode::Percent(pct) => pct,
            _ => 100,
        };
        let next = if delta > 0 {
            SCALE_PRESETS
                .iter()
                .copied()
                .find(|&p| p > current)
                .unwrap_or(current.max(*SCALE_PRESETS.last().unwrap()))
        } else if delta < 0 {
            SCALE_PRESETS
                .iter()
                .rev()
                .copied()
                .find(|&p| p < current)
                .unwrap_or(current.min(*SCALE_PRESETS.first().unwrap()))
        } else {
            current
        };
        ScaleMode::Percent(next)
    }
}

impl fmt::Display for ScaleMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScaleMode::Native => f.write_str("native"),
            ScaleMode::Fit => f.write_str("fit"),
            ScaleMode::Integer => f.write_str("integer"),
            ScaleMode::OneToOne => f.write_str("1:1"),
            ScaleMode::Percent(pct) => write!(f, "{pct}%"),
        }
    }
}

impl FromStr for ScaleMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("native") {
            return Ok(ScaleMode::Native);
        }
        if trimmed.eq_ignore_ascii_case("fit") {
            return Ok(ScaleMode::Fit);
        }
        if trimmed.eq_ignore_ascii_case("integer") {
            return Ok(ScaleMode::Integer);
        }
        if trimmed == "1:1" {
            return Ok(ScaleMode::OneToOne);
        }
        if let Some(num) = trimmed.strip_suffix('%') {
            let pct: u16 = num.parse().map_err(|_| {
                format!(
                    "invalid scale `{s}`; expected native, fit, integer, 1:1, or a percentage like 125%"
                )
            })?;
            if !(MIN_SCALE_PERCENT..=MAX_SCALE_PERCENT).contains(&pct) {
                return Err(format!(
                    "scale percentage `{s}` out of range; expected {MIN_SCALE_PERCENT}%..={MAX_SCALE_PERCENT}%"
                ));
            }
            return Ok(ScaleMode::Percent(pct));
        }
        Err(format!(
            "invalid scale `{s}`; expected native, fit, integer, 1:1, or a percentage like 125%"
        ))
    }
}

impl Transfer {
    /// Pick a concrete medium.
    ///
    /// Shared memory needs two things: a terminal that answered the `t=s` probe,
    /// and a terminal on this machine. Over SSH the object would be created on the
    /// wrong side of the connection, and an unprobed terminal would fail silently,
    /// because frames go out with responses suppressed.
    pub fn resolve(self, caps: &crate::term::caps::Caps) -> crate::render::Transfer {
        match self {
            Transfer::Direct => crate::render::Transfer::Direct,
            // An explicit request is honoured, but say so if the terminal never
            // agreed to it.
            Transfer::Shm => {
                if !caps.shm_graphics {
                    tracing::warn!(
                        "shared memory was requested but the terminal did not \
                         answer the t=s probe; expect a blank screen"
                    );
                }
                if !crate::term::shm::terminal_is_local() {
                    tracing::warn!(
                        "shared memory was requested but this looks like an ssh \
                         session; the object would be created on the wrong machine"
                    );
                }
                crate::render::Transfer::Shm
            }
            // Shared memory whenever it is actually available, because the gap is
            // not small. Measured on a full-screen 1600x832 update in 91 tiles
            // (`cargo test --release --test perf -- --ignored --nocapture`):
            //
            //   pack BGRA->RGB into a buffer     1.1 ms/frame
            //   direct: +zlib +base64           14.0 ms/frame   ->   71 fps ceiling
            //   shm: object per tile             1.6 ms/frame   ->  633 fps ceiling
            //   shm: per tile, packed in place   0.9 ms/frame   -> 1124 fps ceiling
            //
            // zlib is the whole story on the direct path. On the shared memory one it
            // is the pack: writing into the mapping rather than into a buffer for a
            // copy to follow is most of the difference between the last two rows. One
            // object for the whole frame would be faster still -- 0.7 ms/frame, also
            // measured -- but an object can be placed only once, because the terminal
            // unlinks what it reads; see RENDERING.md. The probe is what makes this
            // safe to default to --
            // frames go out with responses suppressed, so a medium the terminal
            // cannot handle would fail invisibly, and silence about `t=s` sends us
            // down the path that always works.
            Transfer::Auto => {
                if caps.shm_graphics && crate::term::shm::terminal_is_local() {
                    crate::render::Transfer::Shm
                } else {
                    crate::render::Transfer::Direct
                }
            }
        }
    }
}

/// Which Kitty graphics transmission medium to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transfer {
    /// Whatever is known to work everywhere, currently the same as `direct`.
    Auto,
    /// Base64 in the escape sequence itself. Works everywhere, including SSH.
    Direct,
    /// POSIX shared memory: skips base64 and compression, at six syscalls per
    /// tile. Worth it for heavy motion on a local terminal, and useless over SSH,
    /// where the object would be created on the wrong machine.
    Shm,
}

impl Args {
    pub fn from_env() -> Self {
        // TigerVNC viewers spell this long option with a single dash.
        let mut options = true;
        Self::parse_from(std::env::args_os().map(|arg| {
            if arg == "--" {
                options = false;
            }
            if options && arg == "-passwd" {
                "--passwd".into()
            } else {
                arg
            }
        }))
    }

    /// Resolve the `host[:display]` argument into a socket address string.
    pub fn server_addr(&self) -> anyhow::Result<String> {
        let spec = self
            .server
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no server given"))?;
        parse_server(spec)
    }
}

/// Turn a VNC server spec into `host:port`.
///
/// * `host`         -> `host:5900`
/// * `host:1`       -> `host:5901`   (display number, as per convention)
/// * `host:5901`    -> `host:5901`   (values >= 5900 are taken as ports)
/// * `host::5901`   -> `host:5901`   (explicit port)
/// * `[::1]:1`      -> `[::1]:5901`  (bracketed IPv6)
fn parse_server(spec: &str) -> anyhow::Result<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        anyhow::bail!("empty server address");
    }

    // `fe80::1` is a valid IPv6 address and also looks exactly like
    // `host::port`. Resolve the ambiguity by testing for an address first: no
    // hostname can parse as one, so this only ever wins when it should.
    if spec.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok(format!("[{spec}]:5900"));
    }

    // Split off the host part first so IPv6 colons don't confuse us.
    let (host, rest) = if let Some(stripped) = spec.strip_prefix('[') {
        let end = stripped
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("unterminated IPv6 address in `{spec}`"))?;
        (&stripped[..end], &stripped[end + 1..])
    } else {
        match spec.split_once(':') {
            // A bare IPv6 literal has more than one colon and no brackets.
            Some((_, tail)) if tail.contains(':') && !tail.starts_with(':') => (spec, ""),
            Some((host, _)) => (host, &spec[host.len()..]),
            None => (spec, ""),
        }
    };

    if host.is_empty() {
        anyhow::bail!("missing host in `{spec}`");
    }

    let port = if let Some(explicit) = rest.strip_prefix("::") {
        explicit
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("invalid port `{explicit}` in `{spec}`"))?
    } else if let Some(display) = rest.strip_prefix(':') {
        let n: u32 = display
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid display or port `{display}` in `{spec}`"))?;
        if n >= 5900 {
            u16::try_from(n).map_err(|_| anyhow::anyhow!("port {n} out of range"))?
        } else {
            u16::try_from(5900 + n).map_err(|_| anyhow::anyhow!("display {n} out of range"))?
        }
    } else if rest.is_empty() {
        5900
    } else {
        anyhow::bail!("trailing junk `{rest}` in `{spec}`");
    };

    if host.contains(':') {
        Ok(format!("[{host}]:{port}"))
    } else {
        Ok(format!("{host}:{port}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{ScaleMode, parse_server};

    #[test]
    fn bare_host_defaults_to_5900() {
        assert_eq!(parse_server("desk").unwrap(), "desk:5900");
    }

    #[test]
    fn single_colon_is_a_display_number() {
        assert_eq!(parse_server("desk:1").unwrap(), "desk:5901");
        assert_eq!(parse_server("desk:0").unwrap(), "desk:5900");
    }

    #[test]
    fn large_single_colon_values_are_ports() {
        assert_eq!(parse_server("desk:5901").unwrap(), "desk:5901");
    }

    #[test]
    fn double_colon_is_an_explicit_port() {
        assert_eq!(parse_server("desk::5999").unwrap(), "desk:5999");
        assert_eq!(parse_server("desk::80").unwrap(), "desk:80");
    }

    #[test]
    fn ipv6_needs_brackets_for_a_display() {
        assert_eq!(parse_server("[::1]:1").unwrap(), "[::1]:5901");
        assert_eq!(parse_server("[::1]::5900").unwrap(), "[::1]:5900");
        // Unbracketed IPv6 is accepted as a host with the default port.
        assert_eq!(parse_server("fe80::1").unwrap(), "[fe80::1]:5900");
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse_server("").is_err());
        assert!(parse_server(":1").is_err());
        assert!(parse_server("desk:abc").is_err());
        assert!(parse_server("[::1").is_err());
    }

    #[test]
    fn the_scale_modes_cycle_and_come_back_round() {
        let mut mode = ScaleMode::Native;
        let mut seen = vec![mode];
        for _ in 0..3 {
            mode = mode.next();
            seen.push(mode);
        }
        assert_eq!(
            seen,
            vec![
                ScaleMode::Native,
                ScaleMode::Fit,
                ScaleMode::Integer,
                ScaleMode::OneToOne,
            ]
        );
        assert_eq!(mode.next(), ScaleMode::Native);
        assert_eq!(ScaleMode::Percent(125).next(), ScaleMode::Native);
    }

    #[test]
    fn scale_mode_parses_names_and_percentages() {
        for (raw, want) in [
            ("native", ScaleMode::Native),
            ("Native", ScaleMode::Native),
            ("fit", ScaleMode::Fit),
            ("integer", ScaleMode::Integer),
            ("1:1", ScaleMode::OneToOne),
            ("80%", ScaleMode::Percent(80)),
            ("100%", ScaleMode::Percent(100)),
            ("110%", ScaleMode::Percent(110)),
            ("115%", ScaleMode::Percent(115)),
            ("125%", ScaleMode::Percent(125)),
            ("150%", ScaleMode::Percent(150)),
            ("200%", ScaleMode::Percent(200)),
            ("25%", ScaleMode::Percent(25)),
            ("400%", ScaleMode::Percent(400)),
        ] {
            let parsed: ScaleMode = raw.parse().unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(parsed, want);
            assert_eq!(parsed.to_string().parse::<ScaleMode>().unwrap(), want);
        }

        for bad in ["", "0%", "10%", "401%", "125", "abc%", "-10%", "zoom"] {
            assert!(bad.parse::<ScaleMode>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn step_percent_walks_the_presets_without_reversing_past_bounds() {
        assert_eq!(ScaleMode::Native.step_percent(1), ScaleMode::Percent(110));
        assert_eq!(ScaleMode::Native.step_percent(-1), ScaleMode::Percent(80));
        assert_eq!(
            ScaleMode::Percent(80).step_percent(-1),
            ScaleMode::Percent(80)
        );
        assert_eq!(
            ScaleMode::Percent(80).step_percent(1),
            ScaleMode::Percent(100)
        );
        assert_eq!(
            ScaleMode::Percent(110).step_percent(1),
            ScaleMode::Percent(125)
        );
        assert_eq!(
            ScaleMode::Percent(115).step_percent(1),
            ScaleMode::Percent(125)
        );
        assert_eq!(
            ScaleMode::Percent(115).step_percent(-1),
            ScaleMode::Percent(110)
        );
        assert_eq!(
            ScaleMode::Percent(200).step_percent(1),
            ScaleMode::Percent(200)
        );
        assert_eq!(
            ScaleMode::Percent(300).step_percent(1),
            ScaleMode::Percent(300)
        );
        assert_eq!(
            ScaleMode::Percent(300).step_percent(-1),
            ScaleMode::Percent(200)
        );
        assert_eq!(
            ScaleMode::Percent(50).step_percent(1),
            ScaleMode::Percent(80)
        );
        assert_eq!(
            ScaleMode::Percent(50).step_percent(-1),
            ScaleMode::Percent(50)
        );
    }
}
