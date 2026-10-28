name := 'adaptive-dock-icons'
appid := 'io.github.motixo.AdaptiveDockIcons'
prefix := env('HOME') / '.local'
rootdir := ''
base := clean(rootdir / prefix)
target := env('CARGO_TARGET_DIR', 'target')

default: build-release

build-release:
    cargo build --release --locked

build-debug:
    cargo build --locked

check:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked

run: build-release
    {{quote(target / 'release' / name)}}

install: build-release
    install -Dm0755 {{quote(target / 'release' / name)}} {{quote(base / 'bin' / name + '.new')}}
    mv -f {{quote(base / 'bin' / name + '.new')}} {{quote(base / 'bin' / name)}}
    install -Dm0644 {{quote('resources' / appid + '.desktop')}} {{quote(base / 'share/applications' / appid + '.desktop')}}
    install -Dm0644 resources/icon.svg {{quote(base / 'share/icons/hicolor/scalable/apps' / appid + '.svg')}}
    install -Dm0644 {{quote('resources' / appid + '.metainfo.xml')}} {{quote(base / 'share/metainfo' / appid + '.metainfo.xml')}}

uninstall:
    rm -f {{quote(base / 'bin' / name)}} {{quote(base / 'share/applications' / appid + '.desktop')}} {{quote(base / 'share/icons/hicolor/scalable/apps' / appid + '.svg')}} {{quote(base / 'share/metainfo' / appid + '.metainfo.xml')}}
