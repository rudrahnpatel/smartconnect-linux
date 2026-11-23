# Mirror, app and virtual-phone windows: follow the phone's rotation

## What

When the phone turns an app (or the virtual phone) to landscape, the PC window turns too: the
picture is upright, the window changes from portrait to landscape, and mouse input still lands
where you point. Turning back restores it.

## How

- `mirror/src/rotation.rs`: pure helpers. `direction(rotation, mode)` gives the `videoflip`
  `video-direction` (1 -> `90l`, 2 -> `180`, 3 -> `90r`, 0 -> identity), `is_turned` says whether
  the window should swap its sides, `mode` reads `SMARTCONNECT_MIRROR_ROTATION` (`off`, `invert`).
- `mirror/src/main.rs`: the decode chain is `... ! videoconvert ! videoflip name=flip !
  taginject ! <sink>`. `cmd 16` (`Session::set_rotation`) sets `video-direction`; a rotation that
  arrives before the video applies from the first frame. `Session::sync_window_shape` runs on the
  host tick and swaps the window's width and height through X11 (`X11::find_window`,
  `resize_window`) when the window doesn't already have the turned shape.
- Input: the navigation probe sits on `videoflip`'s **src** pad, so it sees the sink's pointer
  positions before `videoflip` maps them back into the sent frame.

## Why

- **The phone doesn't rotate the frame.** A capture of a virtual-phone session with a landscape
  video app showed 720x1584 for the whole session (one video caps event in 18 MB of GStreamer event
  logs), and `cmd 16 rotation=1` with no new `cmd 3`. The old assumption (dropping the
  `video-orientation` extension makes the phone rotate frames itself) was wrong for this phone.
- **Input on the src pad.** First version tapped after `videoflip`: the sent frame's coordinates
  (0..720 wide) were used by the phone as positions in the upright 1584x720 display, so only the
  left 30 to 40 % of the window worked. Positions before the flip are the upright picture's.
- **Window shape from the host tick.** `xvimagesink` sometimes reshapes its own window when the
  picture's shape changes and sometimes doesn't; the check on the window's current shape avoids a
  double swap. The sink's window has the title plus ` : smartconnect-mirror`, and the window
  manager may wrap it in a frame with the same title, so the lookup matches the title as a prefix
  and takes the innermost window.
- Always on rather than opt-in: with rotation 0 `videoflip` is a pass-through, and the phone's own
  reports decide when it turns.

## Verified / not

- Verified on a motorola edge 70 max: virtual phone, landscape video app, rotation 0 to 1 and back,
  clicks across the whole window.
- Not verified: rotations 2 and 3 (direction is inferred), the whole-screen mirror, app streaming
  windows, `glimagesink` (the default sink; the test machine used `xvimagesink`).
