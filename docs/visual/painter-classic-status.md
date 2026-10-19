# Kalium Painter-style visual pass

**Status: IN_PROGRESS. Screenshot acceptance is incomplete.**

## Canonical reference and current capture

The brief's 1920×1080 Painter 2026 screenshot is the canonical composition.
All five supplied references were opened at original resolution. The earlier
Kalium split-view image is a baseline, not the target preset.

- Current app: [1920×1080 capture](kalium-painter-1920x1080.png).
- Close-ups: [Assets](captures/assets.png), [toolbar](captures/toolbar.png),
  [Texture Sets](captures/texture-set-list.png), [Layers](captures/layers.png),
  [Properties](captures/properties.png).
- States: [File menu](captures/file-menu-open.png),
  [numeric editing](captures/toolbar-numeric-editing.png),
  [dock shortcut hover](captures/dock-shortcut-hover.png),
  [Activity shortcut result](captures/dock-shortcut-activity.png).
- A full-resolution reference/app comparison was inspected at
  `/tmp/kalium-painter-2026-comparison.png`. Adobe screenshots remain outside
  the repository; no Adobe art, presets, icons or branding are bundled.

Capture: Xvfb :98, 1920×1080 client area, 1× scale, Vulkan/NVIDIA preview,
clean-built debug executable, isolated user configuration. The initial run
exposed an older executable despite Cargo reporting Fresh; only post-clean
captures were accepted. There is no window manager/titlebar in this capture.

## Current geometry and controls

| Region | Current state | Remaining difference |
|---|---|---|
| Overall | 38 px left/right rails, approximately 198 px Assets, dominant 3D canvas, approximately 312 px inspector | Canvas tab header adds a vertical band compared with Painter; native window/menu chrome differs |
| Canvas | 3D is initially selected; UV remains a sibling tab. Display fills the dock with bounded matching CPU/GPU buffers | Simple cube and basic preview differ from the reference model; this is not PBR parity |
| Right stack | Texture Sets occupy about 18.5%; Layers/Texture Settings above Properties | Existing technical controls differ in content and density; long contextual tabs can require horizontal scrolling |
| Toolbar | Small value fields above thin tracks; original Size/Flow/Hardness/Spacing bindings preserved | Viewport controls remain text-heavy; no unsupported controls were fabricated |
| Layers | Larger square checker/color/mask previews, original eye glyph, blue row outline, separate blend and opacity | Sparse default one-layer project; nested/drag states still need visual comparison |
| Properties | One contextual tab heading, near-square curved stroke and sphere previews | Preview is illustrative of active values, not Painter material rendering; fewer controls |
| Assets | Compact two-column native preset grid | Only two bundled brushes; no material/alpha shelf or copied presets to fill the screenshot |

Existing saved workspaces retain user preferences. Use **Window → Restore
default workspace** to apply the new preset. Numeric Size was edited to 64
and restored to 22 in the live app; the right Activity shortcut was clicked and
Properties restored. These observed interactions supplement the headless tests.

## Verification and limits

The final test/lint/compile results are recorded in the QA matrix and handoff.
Build success does not establish visual completion. Pressed, keyboard focus,
disabled, group collapse, layer drag/drop, scrollbar and tooltip states still
need systematic comparison. Earlier `open-recent-*`, `legacy-recovery-*` and
`split-layout-comparison.png` captures are historical functional evidence and
show the earlier workspace, not this preset. The legacy crash test recovered
identical sampled paint regions without modifying its source archive.

The owner has now prioritized real editable SPP open/save. Further visual work
is deferred behind that format work; this pass remains explicitly incomplete.
