# pacrat — convenience targets. The gates themselves live in CI
# (.github/workflows) and are run with cargo; nothing here redefines them.
.PHONY: help build demo

CARGO ?= cargo

help: ## This list
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-8s\033[0m %s\n", $$1, $$2}'

build: ## Release binary — what the demo drives
	$(CARGO) build --release

# Re-record the README's demo. Safe to run at any time: docs/demo/record.sh
# builds a throwaway store under /tmp and runs every pacrat invocation with
# `env -i`, DOTFILES_DIR and the XDG paths pointed at it — the operator's real
# store is never read and never written, and nothing here calls sudo.
demo: build ## Re-record docs/demo/pacrat.cast and render the gif
	asciinema rec --overwrite --headless --window-size 100x30 \
	  --title 'pacrat — vendor, grade, build, serve, update' \
	  -c ./docs/demo/record.sh docs/demo/pacrat.cast
	@# No --idle-time-limit: record.sh's pauses are the choreography, and
	@# capping them cuts every hold that exists so the frame can be read.
	agg --font-size 14 --line-height 1.3 \
	  docs/demo/pacrat.cast docs/demo/pacrat.gif
	@ls -lh docs/demo/pacrat.gif
