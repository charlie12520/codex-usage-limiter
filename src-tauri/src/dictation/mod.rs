// The limiter has no microphone UI; the whisper/cpal dictation stack is dead
// code here and its native build chain (libclang + CMake) was the single
// biggest obstacle for anyone building from source. All platforms use the
// stub, which keeps the command surface intact.
#[path = "stub.rs"]
mod imp;

pub(crate) use imp::*;
