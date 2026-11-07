IMAGE ?= agent-browser-use
NAME  ?= agent-browser-use
PORT  ?= 3000
# 127.0.0.1 = only this machine can open the viewer and /mcp.
# Use HOST=0.0.0.0 to share the viewer on your network (the MCP endpoint is then reachable from it too).
HOST  ?= 127.0.0.1
ARGS  ?=

.PHONY: docker docker-build docker-run docker-stop docker-logs

## Build the image, then run it on http://localhost:$(PORT)
docker: docker-build docker-run

docker-build:
	docker build -t $(IMAGE) .

## Replaces a running container; cookies/logins stay in the "$(NAME)-data" volume.
docker-run: docker-stop
	docker run -d --name $(NAME) --init \
		--shm-size=1g \
		-p $(HOST):$(PORT):3000 \
		-v $(NAME)-data:/data \
		$(IMAGE) $(ARGS)
	@echo "Running: http://localhost:$(PORT)  (MCP: http://localhost:$(PORT)/mcp)"

docker-stop:
	-@docker rm -f $(NAME) >/dev/null 2>&1

docker-logs:
	docker logs -f $(NAME)
