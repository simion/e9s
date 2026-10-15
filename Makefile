# make run [ARGS="-p prod -r us-west-1"]   build and launch
# make install                             a `s9s` launcher in ~/.local/bin (see install.sh)
# make test
# make shots                              shots/*.html to PNG (headless Chrome)

CARGO ?= $(shell command -v cargo || ls $$HOME/.cargo/bin/cargo /opt/homebrew/opt/rustup/bin/cargo /usr/local/opt/rustup/bin/cargo 2>/dev/null | head -n 1)
# Rustup's proxies find rustc through PATH.
export PATH := $(dir $(CARGO)):$(PATH)

.PHONY: run install uninstall test shots

run:
	$(CARGO) run --release --locked -- $(ARGS)

install:
	./install.sh

uninstall:
	./install.sh --uninstall

test:
	$(CARGO) test --locked

# Render every shots/*.html (from --script @shot=shots/<name>) to a PNG with headless Chrome.
CHROME ?= /Applications/Google Chrome.app/Contents/MacOS/Google Chrome
shots:
	cd shots && for h in *.html; do "$(CHROME)" --headless=new --disable-gpu --hide-scrollbars \
	  --window-size=1660,1010 --screenshot="$${h%.html}.png" "file://$$PWD/$$h" >/dev/null 2>&1; done; ls *.png | wc -l
