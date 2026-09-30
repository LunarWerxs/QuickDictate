# The CI test step's cargo runner (CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER): runs the test
# binary with libtest's per-test wall time on, so the job can print its 30 slowest tests.
#
# `--report-time` is an unstable libtest option, so RUSTC_BOOTSTRAP is set for the test binary
# alone: nothing is compiled under it (cargo builds before it runs any runner). Build scripts
# and other binaries run untouched; only a path with a \deps\ segment is a test binary.
# Every argument passes through and the exit code is the binary's.
$exe = $args[0]
$rest = @($args | Select-Object -Skip 1)
if ($exe -notmatch '[\\/]deps[\\/]') {
    & $exe @rest
    exit $LASTEXITCODE
}
$env:RUSTC_BOOTSTRAP = '1'
if ($env:RT_TEST_TIMES) {
    & $exe @rest -Zunstable-options --report-time | Tee-Object -FilePath $env:RT_TEST_TIMES -Append
} else {
    & $exe @rest -Zunstable-options --report-time
}
exit $LASTEXITCODE
