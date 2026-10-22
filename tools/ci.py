"""What CI runs on every push, from a fresh clone (Windows or Linux): fmt, build, clippy and the
unit tests. Nothing to set up first: on Windows the build links the audio-only FFmpeg in
crates/player-core/third_party/ffmpeg (tools/ffmpeg-audio/build.sh makes it from FFmpeg's source). Tests that need a device or a person's recordings are
#[ignore]d (L5, docs in each). Integration tests and their fixtures are maintained separately.

  python tools/ci.py
"""
import subprocess, sys, time, os

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def step(name, cmd):
    print(f"== {name}: {' '.join(cmd)}", flush=True)
    t0 = time.time()
    r = subprocess.run(cmd, cwd=REPO)
    print(f"== {name}: {'ok' if r.returncode == 0 else 'FAILED'} in {time.time() - t0:.0f}s", flush=True)
    if r.returncode != 0:
        sys.exit(r.returncode)


def main():
    step("fmt", ["cargo", "fmt", "--all", "--", "--check"])
    step("build", ["cargo", "build", "--release", "--workspace"])
    step("clippy", ["cargo", "clippy", "--release", "--workspace", "--all-targets", "--", "-D", "warnings"])
    step("tests", ["cargo", "test", "--release", "--workspace"])
    print("ci: all passed")


if __name__ == "__main__":
    main()
