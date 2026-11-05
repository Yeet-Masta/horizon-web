@echo off
rem Builds the web version of the game into web\dist on Windows (web\README.md); web/build.sh
rem is the same for Linux and macOS.
rem
rem   web\build.bat                wasm32 with threads, the default
rem   web\build.bat --wasm64       wasm64 (memory64): up to 8 GiB of memory instead of 4, and slower
rem   web\build.bat --no-threads   one thread: no shared memory, so no COOP/COEP headers needed
rem   web\build.bat --fast         the quicker-to-build `fast` profile (no LTO) instead of release
rem   web\build.bat --config FILE  adds a cargo config (rustflags, say), as web\simd.toml does
rem   web\build.bat --abort        panics abort the thread they happen on instead of unwinding
rem                                (web\unwind.toml; the one-thread wasm32 build always aborts)
rem   web\build.bat --profiling    keeps the module's function names, which wasm-opt drops, for a
rem                                profiler (web\README.md, "Profiling"): 8 MB more, 2 MB downloaded
rem   web\build.bat --sccache      compiles through sccache (RUSTC_WRAPPER), which keeps compiled
rem                                crates between builds and checkouts: not the game's own crate nor
rem                                its link, nor `fast`'s incremental crates
rem   web\build.bat --cranelift    compiles build scripts and proc macros with the Cranelift backend
rem                                (nightly; rustup adds it). Cranelift can't make WebAssembly: the
rem                                game itself is still LLVM's
rem
rem Every build uses WebAssembly SIMD and tail calls (web\simd.toml), which every browser with
rem WebGPU has. Threads and wasm64 build std here, with nightly Rust (rustup adds rust-src). Needs
rem the wasm-bindgen CLI from the commit Cargo.lock pins (the error says the command); wasm-opt
rem (binaryen) and brotli are used when they are on the PATH. Serve the result with:
rem python web\serve.py
setlocal EnableExtensions EnableDelayedExpansion

set "here=%~dp0"
set "here=%here:~0,-1%"
for %%r in ("%here%\..") do set "root=%%~fr"
set "target=wasm32-unknown-unknown"
set "profile=release"
set "threads=1"
set "unwind=1"
set "names=0"
set "sccache=0"
set "cranelift=0"
set "extra="

:args
if "%~1"=="" goto args_done
if /i "%~1"=="--wasm32" (set "target=wasm32-unknown-unknown" & shift & goto args)
if /i "%~1"=="--wasm64" (set "target=wasm64-unknown-unknown" & shift & goto args)
if /i "%~1"=="--release" (set "profile=release" & shift & goto args)
if /i "%~1"=="--fast" (set "profile=fast" & shift & goto args)
if /i "%~1"=="--no-threads" (set "threads=0" & shift & goto args)
if /i "%~1"=="--threads" (set "threads=1" & shift & goto args)
if /i "%~1"=="--abort" (set "unwind=0" & shift & goto args)
if /i "%~1"=="--profiling" (set "names=1" & shift & goto args)
if /i "%~1"=="--sccache" (set "sccache=1" & shift & goto args)
if /i "%~1"=="--cranelift" (set "cranelift=1" & shift & goto args)
rem cmd splits --config=FILE at the `=`: both spellings arrive as --config then FILE.
if /i "%~1"=="--config" (
  if "%~2"=="" (echo --config needs a file 1>&2 & exit /b 2)
  set "extra=!extra! --config %~2"
  shift & shift & goto args
)
if /i "%~1"=="-h" goto usage
if /i "%~1"=="--help" goto usage
echo unknown option: %~1 1>&2
exit /b 2
:usage
echo web\build.bat [--wasm64] [--no-threads] [--fast] [--config FILE] [--abort] [--profiling]
echo               [--sccache] [--cranelift]
echo   default: wasm32 with threads on nightly Rust, release profile, panics that unwind;
echo   --wasm64 for up to 8 GiB; --no-threads for servers without COOP/COEP headers; --fast to
echo   build quicker, without LTO; --config adds a cargo config; --abort for panics that abort;
echo   --profiling keeps function names for a profiler; --sccache compiles through sccache;
echo   --cranelift compiles build scripts and proc macros with Cranelift (not the game: no wasm)
exit /b 0
:args_done

cd /d "%root%" || exit /b 1

rem The wasm-bindgen CLI must match Cargo.lock: wasm-bindgen comes from its main branch for now
rem (Cargo.toml's [patch.crates-io]), so the CLI is built from the same commit.
set "pinned="
set "rev="
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "$l = Get-Content Cargo.lock; $i = [array]::IndexOf($l, 'name = ' + [char]34 + 'wasm-bindgen' + [char]34); if ($i -ge 0) { ($l[$i + 1] -split [char]34)[1] }"`) do set "pinned=%%v"
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "$l = Get-Content Cargo.lock; $i = [array]::IndexOf($l, 'name = ' + [char]34 + 'wasm-bindgen' + [char]34); if ($i -ge 0 -and $l[$i + 2] -match '#([0-9a-f]+)') { $Matches[1] }"`) do set "rev=%%v"
set "install=cargo install wasm-bindgen-cli --version %pinned% --locked"
if defined rev set "install=cargo install wasm-bindgen-cli --git https://github.com/wasm-bindgen/wasm-bindgen --rev %rev% --locked --force"
set "have="
for /f "tokens=2" %%v in ('wasm-bindgen --version 2^>nul') do set "have=%%v"
if not "%have%"=="%pinned%" (
  echo wasm-bindgen %pinned% %rev% is needed ^(found: %have%^): 1>&2
  echo   %install% 1>&2
  exit /b 1
)

if "%sccache%"=="1" (
  where sccache >nul 2>nul
  if errorlevel 1 (
    echo --sccache: sccache isn't on the PATH ^(cargo install sccache --locked^) 1>&2
    exit /b 1
  )
  set "RUSTC_WRAPPER=sccache"
)
rem Cranelift for what runs on this machine during the build: build scripts, proc macros and what
rem they use (the profile's build-override); cargo's codegen-backend option is nightly's.
set "backend="
if "%cranelift%"=="1" (
  rustup component add rustc-codegen-cranelift-preview --toolchain nightly >nul || exit /b 1
  set "backend=-Zcodegen-backend"
  if "%profile%"=="fast" (
    set "CARGO_PROFILE_FAST_BUILD_OVERRIDE_CODEGEN_BACKEND=cranelift"
  ) else (
    set "CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_CODEGEN_BACKEND=cranelift"
  )
)

rem No sound device, controllers or mod runtime in a browser (web\README.md).
set "crate=-p horizon-client --bin horizon-client --no-default-features --config web/simd.toml%extra%"
rem wasm64 is a tier 3 target, and threads need std built with atomics: std is built here, with
rem the nightly toolchain. web\threads.toml adds shared memory and atomics. Builds that make std
rem can unwind (web\unwind.toml, unless --abort); the one-thread wasm32 build, on stable's prebuilt
rem std, aborts.
set "nightly=%threads%"
if "%target%"=="wasm64-unknown-unknown" set "nightly=1"
set "unwinds=0"
if "%nightly%"=="1" if "%unwind%"=="1" set "unwinds=1"
set "config="
if "%threads%"=="1" set "config=--config web/threads.toml"
set "panic_std=panic_abort"
if "%unwinds%"=="1" (
  set "config=%config% --config web/unwind.toml"
  set "panic_std=panic_unwind"
)
if "%nightly%"=="1" (
  rustup component add rust-src --toolchain nightly >nul || exit /b 1
  rustup target add %target% --toolchain nightly >nul 2>nul
  cargo +nightly build -Zbuild-std=std,%panic_std% %backend% %config% --target %target% --profile %profile% %crate% || exit /b 1
) else if "%cranelift%"=="1" (
  rem On nightly for its codegen-backend option, with the prebuilt std as stable's.
  rustup target add wasm32-unknown-unknown --toolchain nightly >nul || exit /b 1
  cargo +nightly build %backend% --target %target% --profile %profile% %crate% || exit /b 1
) else (
  rustup target add wasm32-unknown-unknown >nul || exit /b 1
  cargo build --target %target% --profile %profile% %crate% || exit /b 1
)
if "%sccache%"=="1" sccache --show-stats | findstr /b /c:"Compile requests" /c:"Cache hits" /c:"Cache misses" /c:"Non-cacheable calls"

set "dist=%here%\dist"
if exist "%dist%" rmdir /s /q "%dist%"
mkdir "%dist%\pkg" "%dist%\fonts" || exit /b 1
set "out=target"
if defined CARGO_TARGET_DIR set "out=%CARGO_TARGET_DIR%"
wasm-bindgen --target web --no-typescript --out-dir "%dist%\pkg" "%out%\%target%\%profile%\horizon-client.wasm"
if errorlevel 1 (
  echo If wasm-bindgen says the module's bindgen format differs, install the pinned one: 1>&2
  echo   %install% 1>&2
  exit /b 1
)

where wasm-opt >nul 2>nul
if errorlevel 1 (
  echo wasm-opt not found: the module is not optimized further ^(install binaryen to shrink it^) 1>&2
) else (
  set "memory64="
  if "%target%"=="wasm64-unknown-unknown" set "memory64=--enable-memory64"
  set "atomics="
  if "%threads%"=="1" set "atomics=--enable-threads"
  set "exceptions="
  if "%unwinds%"=="1" set "exceptions=--enable-exception-handling"
  set "debuginfo="
  if "%names%"=="1" set "debuginfo=-g"
  call :optimize || exit /b 1
)

copy /y "%here%\index.html" "%dist%\" >nul || exit /b 1
copy /y "%here%\horizon.js" "%dist%\" >nul || exit /b 1
copy /y "%here%\worker.js" "%dist%\" >nul || exit /b 1
copy /y "%here%\coi.js" "%dist%\" >nul || exit /b 1
copy /y "%here%\apache.htaccess" "%dist%\.htaccess" >nul || exit /b 1
copy /y "docs\media\brand\web\favicon.svg" "%dist%\" >nul || exit /b 1
for %%f in (Geist-Regular.ttf PixelifySans-SemiBold.ttf) do (
  copy /y "crates\horizon-theme\fonts\%%f" "%dist%\fonts\" >nul || exit /b 1
)
for %%f in ("crates\horizon-theme\fonts\OFL-*.txt") do (
  copy /y "%%~f" "%dist%\fonts\" >nul || exit /b 1
)
set "commit=unknown"
for /f %%c in ('git rev-parse --short HEAD 2^>nul') do set "commit=%%c"
set "threads_json=false"
if "%threads%"=="1" set "threads_json=true"
set "unwind_json=false"
if "%unwinds%"=="1" set "unwind_json=true"
>"%dist%\build.json" echo {"target":"%target%","profile":"%profile%","threads":%threads_json%,"simd":true,"tailCall":true,"unwind":%unwind_json%,"commit":"%commit%"}

rem Brotli beside each text and module file (`.br`): web\serve.py and the .htaccess serve it to
rem browsers that accept it (all current ones), with `Content-Encoding: br`, so the module
rem downloads at about a fifth of its size and still compiles as it streams. Quality 11 for a
rem release, 9 (seconds, not minutes) for --fast; BROTLI_QUALITY sets it. Without the `brotli`
rem tool on the PATH the files are served as they are.
where brotli >nul 2>nul
if errorlevel 1 (
  echo brotli not found: the module is served uncompressed ^(install brotli to compress it^) 1>&2
) else (
  set "quality=9"
  if "%profile%"=="release" set "quality=11"
  if defined BROTLI_QUALITY set "quality=%BROTLI_QUALITY%"
  for /r "%dist%" %%f in (*.wasm *.js *.html *.json *.svg) do (
    brotli -f -k -q !quality! --lgwin=24 "%%f" || exit /b 1
  )
  echo web\dist: Brotli at quality !quality!
)

for %%s in ("%dist%\pkg\horizon-client_bg.wasm") do set /a "mb=%%~zs / 1048576"
echo web\dist: %mb% MB module (%target%, %profile%, threads %threads_json%, unwind %unwind_json%)
echo Serve it with: python web\serve.py   (then open http://localhost:8080/)
exit /b 0

:optimize
wasm-opt -O2 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals --enable-reference-types --enable-multivalue --enable-simd --enable-tail-call --enable-extended-const %memory64% %atomics% %exceptions% %debuginfo% "%dist%\pkg\horizon-client_bg.wasm" -o "%dist%\pkg\horizon-client_bg.wasm"
exit /b %errorlevel%
