IMAGE ?= agent-browser-use
NAME  ?= agent-browser-use
PORT  ?= 3000
HOST  ?= 127.0.0.1
ARGS  ?=
export IMAGE NAME PORT HOST

.PHONY: run build docker docker-build docker-run docker-stop docker-logs clean

run:          ## Run from source
	./scripts/run.sh $(ARGS)

build:        ## Build bin/agent-browser-use
	./scripts/build.sh

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
