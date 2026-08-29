@echo off
setlocal EnableExtensions

rem Fast incremental Release build for the Windows x64 plugin.
rem Optional modes: /test, /package, /all

set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
set "BUILD=%ROOT%\build"
set "CMAKE=%ROOT%\tools\cmake\cmake-4.4.2-windows-x86_64\bin\cmake.exe"
set "CPACK=%ROOT%\tools\cmake\cmake-4.4.2-windows-x86_64\bin\cpack.exe"
set "VCVARS=%ProgramFiles(x86)%\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set "CARGO=%USERPROFILE%\.cargo\bin\cargo.exe"
set "VCPKG_PREFIX=%ROOT%\tools\vcpkg\installed\x64-windows"
set "OBS_SOURCE=%ROOT%\tools\obs-source"
set "OBS_IMPORTS=%ROOT%\tools\obs-dev"
set "OBS_RUNTIME=%ProgramFiles%\obs-studio\bin\64bit"
set "FFMPEG_ROOT=%ROOT%\tools\obs-ffmpeg-dev"
set "MODE=%~1"

if "%MODE%"=="" set "MODE=build"
if /I "%MODE%"=="/test" goto mode_ok
if /I "%MODE%"=="/package" goto mode_ok
if /I "%MODE%"=="/all" goto mode_ok
if /I "%MODE%"=="build" goto mode_ok
echo Usage: %~nx0 [/test ^| /package ^| /all]
exit /b 2

:mode_ok
if not exist "%CMAKE%" (
  echo ERROR: Bundled CMake was not found: "%CMAKE%"
  exit /b 1
)
if not exist "%CPACK%" (
  echo ERROR: Bundled CPack was not found: "%CPACK%"
  exit /b 1
)
if not exist "%VCVARS%" (
  echo ERROR: Visual Studio x64 environment script was not found: "%VCVARS%"
  exit /b 1
)
if not exist "%CARGO%" (
  echo ERROR: Cargo was not found: "%CARGO%"
  exit /b 1
)
if not exist "%OBS_SOURCE%\libobs\obs.h" (
  echo ERROR: OBS source headers were not found: "%OBS_SOURCE%"
  exit /b 1
)
if not exist "%OBS_IMPORTS%\obs.lib" (
  echo ERROR: OBS import libraries were not found: "%OBS_IMPORTS%"
  exit /b 1
)
if not exist "%OBS_RUNTIME%\obs.dll" (
  echo ERROR: OBS runtime was not found: "%OBS_RUNTIME%"
  exit /b 1
)
if not exist "%VCPKG_PREFIX%\share\Qt6\Qt6Config.cmake" (
  echo ERROR: Qt6 was not found: "%VCPKG_PREFIX%"
  exit /b 1
)

echo Initializing Visual Studio x64 environment...
call "%VCVARS%" >nul
if errorlevel 1 (
  echo ERROR: Could not initialize the Visual Studio x64 environment.
  exit /b 1
)

echo Configuring the known Release/static-SRT build...
"%CMAKE%" -S "%ROOT%" -B "%BUILD%" -G Ninja ^
  "-DCMAKE_BUILD_TYPE=Release" ^
  "-DCMAKE_PREFIX_PATH=%VCPKG_PREFIX%" ^
  "-DQt6_DIR=%VCPKG_PREFIX%\share\Qt6" ^
  "-DCMAKE_NINJA_CMCLDEPS_RC=0" ^
  "-DOBS_SRTLA_BUILD_TESTS=ON" ^
  "-DOBS_SRTLA_BUILD_VENDOR_SRT=ON" ^
  "-DOBS_SRTLA_REQUIRE_PLUGIN=ON" ^
  "-DOBS_SRTLA_USE_OBS_RUNTIME_DEPS=ON" ^
  "-DOBS_SRTLA_CARGO_EXECUTABLE=%CARGO%" ^
  "-DOBS_SRTLA_OBS_SOURCE_DIR=%OBS_SOURCE%" ^
  "-DOBS_SRTLA_OBS_IMPORT_LIB_DIR=%OBS_IMPORTS%" ^
  "-DOBS_SRTLA_OBS_RUNTIME_DIR=%OBS_RUNTIME%" ^
  "-DOBS_SRTLA_FFMPEG_ROOT=%FFMPEG_ROOT%" ^
  "-DOBS_SRTLA_MBEDTLS_ROOT=%VCPKG_PREFIX%"
if errorlevel 1 (
  echo ERROR: CMake configuration failed.
  exit /b 1
)

echo Building srtla-output incrementally...
"%CMAKE%" --build "%BUILD%" --config Release --target srtla-output --parallel
if errorlevel 1 (
  echo ERROR: Plugin build failed.
  exit /b 1
)

if /I "%MODE%"=="/test" goto test
if /I "%MODE%"=="/all" goto test
if /I "%MODE%"=="/package" goto package
goto success

:test
echo Running tests...
"%CMAKE%" --build "%BUILD%" --config Release --parallel
if errorlevel 1 (
  echo ERROR: Test build failed.
  exit /b 1
)
"%ROOT%\tools\cmake\cmake-4.4.2-windows-x86_64\bin\ctest.exe" --test-dir "%BUILD%" --build-config Release --output-on-failure
if errorlevel 1 (
  echo ERROR: Tests failed.
  exit /b 1
)
if /I "%MODE%"=="/all" goto package
goto success

:package
echo Creating the ZIP package in the build folder...
pushd "%BUILD%"
"%CPACK%" --config "CPackConfig.cmake" -C Release
set "PACKAGE_RC=%ERRORLEVEL%"
popd
if not "%PACKAGE_RC%"=="0" (
  echo ERROR: Package creation failed.
  exit /b %PACKAGE_RC%
)

:success
echo.
echo Build completed successfully.
echo DLL: "%BUILD%\plugin\srtla-output.dll"
if /I "%MODE%"=="/package" echo ZIP: "%BUILD%\srtla-output-0.1.1-obs32-windows-x64.zip"
if /I "%MODE%"=="/all" echo ZIP: "%BUILD%\srtla-output-0.1.1-obs32-windows-x64.zip"
exit /b 0
