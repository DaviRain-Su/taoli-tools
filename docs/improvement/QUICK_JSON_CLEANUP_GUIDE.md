# 🧹 JSON文件清理 - 快速指南

## 🚀 一键使用

### 最常用命令

```bash
# 1. 交互式删除（推荐）
./cleanup_json_files.sh

# 2. 查看有哪些JSON文件
./cleanup_json_files.sh --list

# 3. 自动删除所有JSON文件
./cleanup_json_files.sh --auto

# 4. 查看帮助
./cleanup_json_files.sh --help
```

## 📋 常见场景

### 场景1：重新开始交易
```bash
# 删除所有状态文件，重新开始
./cleanup_json_files.sh --auto
```

### 场景2：清理特定文件
```bash
# 只删除网格状态文件
./cleanup_json_files.sh --pattern "grid_*.json" --auto

# 只删除订单状态文件  
./cleanup_json_files.sh --pattern "orders_*.json" --auto
```

### 场景3：安全删除
```bash
# 先查看文件
./cleanup_json_files.sh --list

# 再交互式删除
./cleanup_json_files.sh
```

## 🔧 脚本文件

- **Shell脚本**: `cleanup_json_files.sh` (推荐)
- **Python脚本**: `cleanup_json_files.py`
- **使用说明**: `README_JSON_CLEANUP.md`

## ⚠️ 重要提醒

1. **自动备份**: 删除前会自动备份到 `backup_deleted_files/` 目录
2. **确认删除**: 交互模式会要求确认，自动模式直接删除
3. **文件恢复**: 可从备份目录恢复文件

## 🎯 目标文件

脚本会查找并删除以下JSON文件：
- `grid_state.json` - 网格状态
- `orders_state.json` - 订单状态  
- `dynamic_grid_params.json` - 动态参数
- `performance_*.json` - 性能数据
- `trading_history_*.json` - 交易历史
- `risk_events_*.json` - 风险事件

---

**💡 提示**: 首次使用建议先运行 `./cleanup_json_files.sh --list` 查看文件列表 