# The Windows daemon is cross-built from Linux with MinGW (scripts/build-daemon-win.sh names this file to CMake through
# CMAKE_TOOLCHAIN_FILE_x86_64_pc_windows_gnu). Every crate built with CMake for that target reads it.
set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_C_COMPILER x86_64-w64-mingw32-gcc)
set(CMAKE_CXX_COMPILER x86_64-w64-mingw32-g++)
set(CMAKE_RC_COMPILER x86_64-w64-mingw32-windres)
set(CMAKE_FIND_ROOT_PATH /usr/x86_64-w64-mingw32)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
# BoringSSL (Signal's number lookup, through libsignal-net): its x86-64 assembly is built for ELF and Mach-O only, while
# its C calls the P-256 ADX routines whenever GCC targets x86-64, so a MinGW build fails to link
# (fiat_p256_adx_mul/sqr). Without the assembly it uses its portable C, as boring-sys already does on a Windows host.
set(OPENSSL_NO_ASM ON CACHE BOOL "" FORCE)
