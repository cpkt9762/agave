# ==================================================================
# Agave RPC Node — Cross-Compile & Deploy
# ==================================================================
# macOS (Apple Silicon) → Linux x86_64 交叉编译 + 远程部署
#
# 用法:
#   make build          # 编译 validator + geyser 插件
#   make deploy         # 热部署（停止 → 替换二进制 → 启动，不重新下载快照）
#   make redo           # 全量重置（停止 → 清理 → 下载快照 → 启动）
#   make status         # 检查节点状态
#   make logs           # 查看最新日志
# ==================================================================

# --- 配置 ---
TARGET           := x86_64-unknown-linux-gnu
VALIDATOR_PROFILE := release-with-lto
GEYSER_PROFILE   := release

SSH_HOST         := sol-server
REMOTE_BIN       := /usr/local/solana/bin/agave-validator
REMOTE_GEYSER    := /root/sol/bin/yellowstone-grpc-geyser-release/lib/libyellowstone_grpc_geyser.so
REMOTE_REDO      := /root/redo_node.sh

YELLOWSTONE_DIR  := $(CURDIR)/geyser-plugin/yellowstone-grpc

# --- 交叉编译环境 (仅 make build 时注入，不污染 native 编译) ---
CROSS_CC         := $(CURDIR)/.cross/x86_64-linux-gnu-gcc
CROSS_CXX        := $(CURDIR)/.cross/x86_64-linux-gnu-g++
CROSS_SYSROOT    := $(CURDIR)/.cross/x86_64-linux-gnu-sysroot
CROSS_RUSTFLAGS  := -Ctarget-cpu=x86-64-v2

CROSS_ENV := \
	CC_x86_64_unknown_linux_gnu=$(CROSS_CC) \
	CXX_x86_64_unknown_linux_gnu=$(CROSS_CXX) \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=$(CROSS_CC) \
	PKG_CONFIG_SYSROOT_DIR=$(CROSS_SYSROOT) \
	PKG_CONFIG_PATH_x86_64_unknown_linux_gnu=$(CROSS_SYSROOT)/usr/lib/x86_64-linux-gnu/pkgconfig

# --- 产物路径 ---
VALIDATOR_BIN    := target/$(TARGET)/$(VALIDATOR_PROFILE)/agave-validator
GEYSER_SO        := $(YELLOWSTONE_DIR)/target/$(TARGET)/$(GEYSER_PROFILE)/libyellowstone_grpc_geyser.so

# ==================================================================
# 高级目标
# ==================================================================

.PHONY: build deploy redo status logs health build-validator build-geyser \
        upload stop start restart clean help

## 编译 validator + geyser 插件
build: build-validator build-geyser
	@echo ""
	@echo "✅ 编译完成"
	@ls -lh $(VALIDATOR_BIN)
	@ls -lh $(GEYSER_SO)

## 热部署：停止 → 上传 → 启动（保留现有快照和数据）
deploy: build upload
	@echo ""
	@echo "==> 停止服务 ..."
	@ssh $(SSH_HOST) "systemctl stop sol" 2>/dev/null || true
	@sleep 2
	@echo "==> 替换二进制 ..."
	@ssh $(SSH_HOST) "\
		cp /tmp/agave-validator $(REMOTE_BIN) && \
		chmod +x $(REMOTE_BIN) && \
		cp /tmp/libyellowstone_grpc_geyser.so $(REMOTE_GEYSER)"
	@echo "==> 启动服务 ..."
	@ssh $(SSH_HOST) "systemctl start sol"
	@sleep 5
	@echo ""
	@echo "✅ 热部署完成"
	@$(MAKE) --no-print-directory health

## 全量重置：编译 → 上传 → redo_node.sh（重新下载快照）
redo: build upload
	@echo ""
	@echo "==> 执行 redo_node.sh (全量重置) ..."
	@ssh $(SSH_HOST) "bash $(REMOTE_REDO)"

## 仅部署 validator（不重新编译 geyser）
deploy-validator: build-validator upload-validator
	@ssh $(SSH_HOST) "systemctl stop sol" 2>/dev/null || true
	@sleep 2
	@ssh $(SSH_HOST) "cp /tmp/agave-validator $(REMOTE_BIN) && chmod +x $(REMOTE_BIN) && systemctl start sol"
	@sleep 5
	@echo "✅ validator 已部署"
	@$(MAKE) --no-print-directory health

## 仅部署 geyser 插件（不重新编译 validator）
deploy-geyser: build-geyser upload-geyser
	@ssh $(SSH_HOST) "systemctl stop sol" 2>/dev/null || true
	@sleep 2
	@ssh $(SSH_HOST) "cp /tmp/libyellowstone_grpc_geyser.so $(REMOTE_GEYSER) && systemctl start sol"
	@sleep 5
	@echo "✅ geyser 插件已部署"
	@$(MAKE) --no-print-directory health

# ==================================================================
# 编译目标
# ==================================================================

## 交叉编译 agave-validator
build-validator:
	@echo "==> 交叉编译 agave-validator ($(VALIDATOR_PROFILE)) ..."
	$(CROSS_ENV) RUSTFLAGS="$(CROSS_RUSTFLAGS)" \
		cargo build --target $(TARGET) --profile $(VALIDATOR_PROFILE) -p agave-validator
	@echo "    ✓ $(VALIDATOR_BIN)"

## 交叉编译 yellowstone-grpc geyser 插件
build-geyser:
	@echo "==> 交叉编译 yellowstone-grpc-geyser ($(GEYSER_PROFILE)) ..."
	$(CROSS_ENV) RUSTFLAGS="$(CROSS_RUSTFLAGS) -Adeprecated" \
		cargo build --manifest-path $(YELLOWSTONE_DIR)/Cargo.toml \
		--target $(TARGET) --profile $(GEYSER_PROFILE) \
		-p yellowstone-grpc-geyser
	@echo "    ✓ $(GEYSER_SO)"

# ==================================================================
# 上传目标
# ==================================================================

upload: upload-validator upload-geyser

upload-validator: $(VALIDATOR_BIN)
	@echo "==> 上传 agave-validator ..."
	scp $(VALIDATOR_BIN) $(SSH_HOST):/tmp/agave-validator

upload-geyser: $(GEYSER_SO)
	@echo "==> 上传 libyellowstone_grpc_geyser.so ..."
	scp $(GEYSER_SO) $(SSH_HOST):/tmp/libyellowstone_grpc_geyser.so

# ==================================================================
# 远程控制
# ==================================================================

## 停止节点
stop:
	@ssh $(SSH_HOST) "systemctl stop sol && echo '✓ 已停止'"

## 启动节点
start:
	@ssh $(SSH_HOST) "systemctl start sol && echo '✓ 已启动'"

## 重启节点（不重新下载快照）
restart:
	@ssh $(SSH_HOST) "systemctl restart sol && echo '✓ 已重启'"

## 检查节点健康状态
health:
	@ssh $(SSH_HOST) 'curl -s --max-time 5 \
		-X POST http://127.0.0.1:8899 \
		-H "Content-Type: application/json" \
		-d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getHealth\"}" \
		2>/dev/null || echo "{\"error\":\"RPC not ready\"}"'
	@echo ""

## 检查追块进度
status:
	@echo "=== 节点状态 ==="
	@ssh $(SSH_HOST) '\
		echo "--- Health ---" && \
		(curl -s --max-time 5 \
			-X POST http://127.0.0.1:8899 \
			-H "Content-Type: application/json" \
			-d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getHealth\"}" 2>/dev/null \
			|| echo "{\"error\":\"RPC not ready\"}") && \
		echo "" && \
		echo "--- Process ---" && \
		(ps aux | grep agave-validator | grep -v grep \
			| awk "{printf \"PID: %s  MEM: %s%%  RSS: %dMB\n\", \$$2, \$$4, \$$6/1024}" \
			|| echo "not running") && \
		echo "--- Log (last 5) ---" && \
		tail -5 /root/sol/agave-rpc.log 2>/dev/null || echo "(no log)"'

## 查看最新日志
logs:
	@ssh $(SSH_HOST) "tail -50 /root/sol/agave-rpc.log"

## 实时跟踪日志（Ctrl+C 退出）
logs-follow:
	@ssh $(SSH_HOST) "tail -f /root/sol/agave-rpc.log"

## 追块进度
catchup:
	@ssh $(SSH_HOST) "/root/catchup.sh"

# ==================================================================
# 清理
# ==================================================================

## 清理本地交叉编译产物
clean:
	cargo clean --target $(TARGET)
	cd $(YELLOWSTONE_DIR) && cargo clean --target $(TARGET)

# ==================================================================
# 帮助
# ==================================================================

help:
	@echo "Agave RPC Node — Cross-Compile & Deploy"
	@echo ""
	@echo "编译 & 部署:"
	@echo "  make build            编译 validator + geyser 插件"
	@echo "  make build-validator  仅编译 validator"
	@echo "  make build-geyser     仅编译 geyser 插件"
	@echo "  make deploy           热部署（停止→替换→启动，保留数据）"
	@echo "  make deploy-validator 仅部署 validator"
	@echo "  make deploy-geyser    仅部署 geyser 插件"
	@echo "  make redo             全量重置（重新下载快照）"
	@echo ""
	@echo "远程控制:"
	@echo "  make stop             停止节点"
	@echo "  make start            启动节点"
	@echo "  make restart          重启节点"
	@echo ""
	@echo "监控:"
	@echo "  make status           节点状态总览"
	@echo "  make health           健康检查"
	@echo "  make catchup          追块进度"
	@echo "  make logs             最新50行日志"
	@echo "  make logs-follow      实时日志（Ctrl+C退出）"
	@echo ""
	@echo "其他:"
	@echo "  make clean            清理交叉编译产物"
