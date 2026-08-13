# Why this fork exists

Branched from `v0.30.13`, one change in `src/platform_impl/android/mod.rs`.

Hover `MotionEvent` actions fell into the catch-all arm, which produces no winit
event and leaves the input status at `Handled`. android-activity then finishes
the event and `ViewRootImpl` consumes it before any `View` sees it, so no
`TYPE_VIEW_HOVER_ENTER` ever leaves the window. Android exposes no positional
hit test on `AccessibilityNodeProvider`, so a pointer can only reach a virtual
accessibility node through a hover event that arrives at the host view.

`HoverEnter`, `HoverMove` and `HoverExit` now leave the input status at
`Unhandled`. Slint's Android backend does the same for enter and exit.

Delete this fork once the change is upstream.
