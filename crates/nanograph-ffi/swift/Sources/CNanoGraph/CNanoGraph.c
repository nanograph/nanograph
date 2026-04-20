// This translation unit exists only so SwiftPM emits a CNanoGraph object
// file. Xcode's SPM integration (Xcode 15+) treats C targets with no
// sources as an error when consuming the package from a .xcodeproj, even
// though `swift build` handles headers-only targets fine. All real
// symbols come from the Rust `libnanograph_ffi` dylib linked in
// NanoGraph's linkerSettings; this file is intentionally empty.

void _cnanograph_anchor(void) {}
