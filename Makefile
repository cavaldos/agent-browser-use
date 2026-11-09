IMAGE ?= agent-browser-use
NAME  ?= agent-browser-use
PORT  ?= 3000
HOST  ?= 127.0.0.1
ARGS  ?=
BINDIR ?= $(HOME)/.local/bin
export IMAGE NAME PORT HOST

# Short options for `make run`:  make run p=8080 pw=secret engine=brave
RUN_FLAGS := $(if $(p),-p $(p)) $(if $(pw),-pw $(pw)) $(if $(engine),-engine $(engine))

# Flags after `--` go straight to the app:  make run -- -p 8080 -pw secret
ifeq (run,$(firstword $(MAKECMDGOALS)))
  RUN_ARGS := $(wordlist 2,$(words $(MAKECMDGOALS)),$(MAKECMDGOALS))
  ifneq ($(RUN_ARGS),)
    $(eval $(RUN_ARGS):;@:)
  endif
endif

.PHONY: run build install test-release release docker docker-build docker-run docker-stop docker-logs clean

run:          ## Run from source
	./scripts/run.sh $(RUN_FLAGS) $(ARGS) $(RUN_ARGS)

build:        ## Build bin/agent-browser-use
	./scripts/build.sh

install: build  ## Install the binary to ~/.local/bin (no root; BINDIR=... to change)
	install -d "$(BINDIR)"
	install -m 755 bin/agent-browser-use "$(BINDIR)/agent-browser-use"
	@echo "Installed: $(BINDIR)/agent-browser-use"

test-release: ## Dry-run the release pipeline locally (add ARGS=--quick for a fast check)
	./scripts/test-release.sh $(ARGS)

release:      ## Tag and push a release: make release V=v1.2.3
	@test -n "$(V)" || { echo "usage: make release V=vX.Y.Z"; exit 1; }
	./scripts/release.sh $(V)

docker: docker-build docker-run   ## Build image and run it

docker-build:
	./scripts/docker-build.sh

docker-run:
	./scripts/docker-run.sh $(ARGS)

docker-stop:
	-@docker rm -f $(NAME) >/dev/null 2>&1

docker-logs:
	docker logs -f $(NAME)

clean:
	rm -rf bin
