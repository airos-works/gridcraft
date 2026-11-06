# GridCraft on air/OS (and Haiku)

Status: first working build 2026-10-17 (0.7.0); synced with upstream 0.16.0 on 2026-11-04; drawn with Vulkan since 2026-11-07 (0.16.0~airos1-2). GridCraft runs as a native air/OS application: the
whole interface (ribbon, Backstage, dialogs, the grid, charts) and everything below it are the
same code as on the other desktops; only the host around the interface is new. The host is the
one WordCraft's air/OS port uses (`storytold/wordcraft`'s fork, `apps/wordcraft-haiku`), adapted
to GridCraft's `SheetApp`.

## How it fits together

| Desktop app (`apps/gridcraft`) | air/OS app (`apps/gridcraft-haiku`) |
| --- | --- |
| eframe + winit window | `BApplication` + `BWindow` + `BView` (`native/host.cpp`) |
| wgpu renders egui's meshes | wgpu/Vulkan renders them into a texture shown in the window (`src/gpu.rs`, `src/present.rs`); a software rasteriser (`src/raster.rs`) without a usable GPU |
| rfd file dialogs | `BFilePanel` Open and Save panels |
| arboard clipboard | `BClipboard` (`text/plain`, `text/html`, PNG and JPEG pictures) |
| craft-print (CUPS, Windows spooler) | the Be print kit (`BPrintJob`, `native/print.cpp`, `src/print.rs`) |
| winit cursors | `BCursor` system cursors |
| xdg-open for links | `BUrl::OpenWithPreferredApplication` (http, https and mailto only, as on the desktop) |
| OS locale | `BLocaleRoster` (preferred languages, formatting conventions) |
| Finder / Explorer open | `B_REFS_RECEIVED` (Tracker double-click, Open With, drops) |

eframe and winit don't run on Haiku, so `SheetApp` (the interface, `crates/ui-egui`) is
driven directly: the host turns Be API messages into egui input, runs `SheetApp::logic` and
`SheetApp::ui` as eframe does, and turns egui's output back into window, cursor, clipboard and
URL calls. The control server and the log file are the desktop app's own modules
(`apps/gridcraft/src/control_server.rs`, `logging.rs`); as on the desktop, a control connection's
first line must carry the run's token (`--control-token`, `GRIDCRAFT_CONTROL_TOKEN`, or the one
the app writes to `~/.cache/gridcraft/control-<port>.token`). Nothing below the interface knows it is
on Haiku apart from the font folders (`/boot/system/data/fonts`, Noto Sans for the interface and
cells, Noto Sans CJK, Thai and Symbols as fallbacks).

Threads: `be_app` runs on the main thread, the window on its looper thread, and the interface
on a thread of its own, which waits on the host's event queue between frames. GridCraft's file
pickers return a path (`Services::pick_open`), so while a file panel is open the interface
thread waits for its answer and keeps the window's other events for afterwards. Only
`src/host.rs` contains `unsafe` code (the FFI calls); the crate denies it everywhere else.

Settings and logs live in `~/config/settings/GridCraft` (`ui.json`, `prefs.json`, `logs/`).
Files GridCraft saves get their MIME type (`update_mime_info`), so Tracker opens them with it;
at start GridCraft becomes the preferred application of the spreadsheet types that have none,
and the package's post-install script does the same before it has ever run.

### Drawing

Since 0.16.0~airos1-2 the window is drawn with Vulkan when the machine has a usable hardware
adapter, as the other Craft hosts on air/OS do (`apps/gridcraft-haiku/src/gpu.rs`): egui's
triangles (the grid, the ribbon, charts, sparklines, pictures) are drawn by egui-wgpu into a
BGRA texture, the same renderer upstream's eframe uses on the other desktops. Haiku has no wgpu
surface, so the presenter (`src/present.rs`, `damage.rs`, `scanout.rs`, shared with WordCraft,
PdfCraft and PhotoCraft) compares the frame with what app_server shows, 64 px tile by tile on
the GPU, and copies back only the tiles that changed. A frame drawn from exactly the same
triangles, size and textures as the one on the screen is not drawn again (egui repaints more
often than its output changes; about 30 % of the frames in the measurement below), so an idle
or hovering window costs almost nothing.

- `GRIDCRAFT_RENDERER=auto|vulkan|cpu`: auto (the default) uses Vulkan on a discrete or
  integrated GPU (a discrete one first) and the rasteriser otherwise; vulkan fails at start
  without one; cpu always rasterises.
- `GRIDCRAFT_PRESENT=readback|full|direct`: readback (the default) as above; full reads the
  whole frame back every time; direct (experimental, not the default until RADV is verified)
  copies frames straight into the screen's frame buffer through air/OS's
  `VkImportScanoutMemoryHAIKU` (the window is a `BDirectWindow`).
- The device asks only for wgpu's downlevel limits, with the adapter's own largest texture size
  (at least 4096 px) and buffer size, and no optional feature, so the smaller Vulkan drivers of
  the arm64 images (V3DV, PowerVR, Turnip, Panfrost) qualify. Every adapter passed over is logged
  with the reason (`GridCraft::gpu: Vulkan adapter … not used: …`), as is the one chosen.
- A failed device creation, a lost device, an uncaptured wgpu error or a panic in a GPU call
  (the first frame included) switches to the rasteriser for good, at once and without losing the
  open workbooks: the rasteriser keeps egui's textures all along.
- wgpu's Vulkan backend is enabled for Haiku by the vendored `wgpu`, `wgpu-core` and
  `wgpu-core-deps-windows-linux-android` 30.0.1 (`vendor/README.md`, from WordCraft).

Upstream's desktop app uses the GPU only through eframe's egui-wgpu renderer (`graphics.rs` only
picks the backend); charts and sparklines are egui shapes, and pictures
egui textures, so all of them are drawn on the GPU here too. PDF export and printing render on
the processor on every platform.

The software rasteriser (`raster.rs`) draws egui's meshes on the processor: axis-aligned
rectangles (cells, panels, glyphs, pictures) and flat triangles take fast paths; the frame is
split into bands of 32 rows drawn in parallel (rayon), and a band whose shapes, clips and
textures are unchanged since the last frame is skipped, so a blinking caret or a keystroke
redraws a few bands, not the window. Only the rectangle that changed is copied into the
`BBitmap` and redrawn. Its worker threads start only when it is the renderer (none while the
GPU draws), at most 4 (`RAYON_NUM_THREADS` overrides): on air/OS more cost more in waking than
they save (8 used 5 % more processor time than 4 in the measurement below).

The window's monitor (`BPrivate::get_display_frame`, an app_server round trip) is remembered
between frames and asked again only when the window moves or the screens change, and the window
is asked for its state once a frame.

Measured on the x399 workstation (GeForce GTX 1070, NVK, 3840 x 2160 at 200 %, a 2880 x 1800 px
window), `--sample sales`, the same 100 inputs through the control channel (arrow keys, Page
Down/Up, 20 cell edits, 40 pointer moves, 60 ms apart):

| | GridCraft CPU | app_server CPU | frame (UI + tessellation + drawing, median / p90) | threads |
| --- | --- | --- | --- | --- |
| 0.16.0~airos1-1 (rasteriser, 8 workers) | 7.04 s | 0.25 s | 2.1 + 0.3 + 4.1 / 7.2 ms | 12 |
| airos1-2, `GRIDCRAFT_RENDERER=cpu` (4 workers) | 6.59 s | 0.03 s | 2.4 + 0.3 + 4.1 / 7.4 ms | 9 |
| airos1-2, Vulkan, readback (default) | 2.27 s | 0.03 s | 2.1 + 0.3 + 5.0 / 5.4 ms | 7 |
| airos1-2, Vulkan, `GRIDCRAFT_PRESENT=direct` | 3.66 s | 0.03 s | | 7 |

Ten idle seconds cost nothing in every case (no frames). The GPU frame's 5 ms is almost all
waiting: encoding takes 0.12 ms, and the presenter waits twice for the GPU (the changed tiles,
then their pixels; a typical interaction frame reads back one 64 px tile), so its latency is a
millisecond above the rasteriser's median on this machine but steadier (p90 5.4 against 7.2 ms,
no 20 ms full redraws), with a third of the processor time. Start-up: the Vulkan device and
renderer are ready about 160 ms after the interface starts.

On an air/OS desktop drawn at a higher density (175 % on the Framework laptop, 200 % on 4K
monitors), the frame is rendered in device pixels and drawn with `DrawBitmap(bitmap, device
rect, logical rect)`, so app_server copies it one to one and text stays sharp. The density comes
from air/OS's display layout (`BPrivate::get_display_layout`, looked up at run time, so plain
Haiku gets 1); the interface scale follows the system font size (12 pt = 100 %) on top of
GridCraft's own interface size setting. `GRIDCRAFT_DEVICE_SCALE` overrides the density.

### Keys

Haiku's Command key (left Alt by default) and Control both act as Excel's Ctrl, so Command+B,
Command+S, Command+C/V work as Haiku users expect and Ctrl+B as Excel users expect. Option (the
Windows key) is Excel's Alt (key tips). `BWindow`'s own Command+X/C/V/A/W shortcuts are removed
so the interface gets them. Letters are identified by the keymap (`raw_char`), so shortcuts
follow the user's layout.

## Building

The Rust code is cross-compiled on Linux with current stable Rust, building `std` for the
tier-3 target `x86_64-unknown-haiku` from `rust-src`:

```sh
# once: rustup with rust-src, and a sysroot of an air/OS machine's development files
export RUSTUP_HOME=/mnt/HaikuWork/toolchains/rust/rustup CARGO_HOME=/mnt/HaikuWork/toolchains/rust/cargo
curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal -c rust-src
# sysroot: /boot/system/develop/{lib,headers} copied (dereferenced) into
# $HAIKU_SYSROOT/boot/system/develop, plus libstdc++/libgcc_s from gcc_syslibs

bash tools/haiku/build.sh          # target/x86_64-unknown-haiku/release/GridCraft
bash tools/haiku/package.sh        # artifacts/haiku/gridcraft-<version>-x86_64.hpkg
```

`tools/haiku/env.sh` sets the toolchain up (the Haiku cross gcc for linking and for
`native/host.cpp`, `RUSTC_BOOTSTRAP=1` for `-Zbuild-std`). The package step needs a Haiku
machine for `rc`, `xres`, `mimeset` and `package`: the test VM, or the workstation with
`GC_PKG_HOST=ws`. `tools/haiku/make-icon.py` generates the original native vector icon
(`GridCraft.hvif`) and a matching SVG preview; see the icon section below.

The platform-neutral parts (the rasteriser, the key tables) are tested anywhere:
`cargo test -p gridcraft-haiku`. Elsewhere the binary only says it needs Haiku.

## Testing

- `tools/haiku/vm/run-vm.sh` boots a throwaway Haiku VM (VNC 127.0.0.1:5910, SSH 2230) as an
  overlay on another project's disk; `tools/haiku/vm/haiku.sh` runs commands in it,
  `tools/haiku/vm/vm.py` takes screenshots and sends clicks, keys and wheel turns, and
  `tools/haiku/vm/ctl.sh` talks to the control channel of a copy started with `--control 7979`
  (e.g. `ui.screenshot`).
- `RUST_LOG=GridCraft*=debug` logs keys, clipboard output and each frame's timings (interface,
  tessellation, rasteriser, present) to `~/config/settings/GridCraft/logs/`.

### Printing

File › Print lists the print server's printers (`~/config/settings/printers`). Print hands the
print-ready PDF to a worker that rasterises each page with hayro at the printer's resolution (at
most 300 dpi) and spools it through a `BPrintJob`; the print server then runs the printer's
driver. Shift+Print (or `GRIDCRAFT_PRINT_SETUP=1`) opens the driver's page setup and job setup
first; their answers are kept per printer in `~/config/settings/GridCraft/printers`. The module
is the one the other Craft hosts share (WordCraft's, with the `gc_print_` prefix).

### Updates

air/OS installs and updates GridCraft as a package (pkgman, SoftwareUpdater), so the start-up
update check never runs and Check for Updates only says so.

## Known gaps

- One window: upstream's desktop app (0.16) gives each workbook window a native window of its own
  (`doc_windows`, craft-window); here every workbook shares the one `BWindow`, which shows the
  active one (View › Switch Windows, the Window list), as in the web build.
- No input methods (`B_INPUT_METHOD_EVENT`) yet, so CJK input methods don't reach the grid.
- The window's size and position aren't remembered between runs, and fullscreen and the
  maximized state aren't reported (Haiku has zoom, not maximize).
- Direct presentation is not the default yet. RADV on the Framework 13 lost the device a few
  seconds after the frame buffer import: re-showing the last frame made an empty submission,
  whose wait the old air/OS RADV could not do (no sync files; fixed in mesa-amdgpu 1a02b91f).
  The presenter no longer makes empty submissions, and a frame within one pixel of the window
  (175 % rounding) still goes direct (2026-11-07); the default flips once the laptop has
  verified it for every Craft host.

## Native application icon (2026-10-26)

The air/OS icon is an original green spreadsheet ledger, selected cell and chart, licensed under
MIT OR Apache-2.0. Its 64-unit vector drawing follows the
[Haiku icon guidelines](https://www.haiku-os.org/development/icon-guidelines):
vertical edges stay upright in perspective, the main light comes from the top
left, the right faces are darker, a floor shadow extends to the right, and the
silhouette has a two-unit black outline (one pixel at 32px). Broad details are
intended to remain legible at 16px and 32px. No fonts or third-party artwork are used.

Rebuild both the native HVIF and its SVG preview from this checkout:

```sh
python3 tools/haiku/make-icon.py
```

`apps/gridcraft-haiku/resources/GridCraft.hvif` is the packaged icon;
`apps/gridcraft-haiku/resources/GridCraft.svg` exports the same geometry for inspection.
The existing `.rdef` imports the HVIF, and `tools/haiku/package.sh` embeds it in
the executable's resources. Tracker and Deskbar therefore use the vector icon.
The upstream icons for other platforms remain separate. Native previews can be
rendered on Haiku with `BIconUtils::GetVectorIcon`; review at 16, 32, 64 and 128px
on light and dark backgrounds before changing the drawing.

Package version `0.7.0~airos1-4` introduced this icon.
