# 🧹 JSON文件清理脚本使用说明

## 📋 概述

这些脚本用于手动删除网格交易策略产生的JSON状态文件，包括：
- `grid_state.json` - 网格状态文件
- `orders_state.json` - 订单状态文件  
- `dynamic_grid_params.json` - 动态参数文件
- `performance_*.json` - 性能数据文件
- `trading_history_*.json` - 交易历史文件
- `risk_events_*.json` - 风险事件文件

## 🛠️ 可用脚本

### 1. Shell脚本 (推荐)
```bash
./cleanup_json_files.sh
```

### 2. Python脚本
```bash
python3 cleanup_json_files.py
```

## 🚀 使用方法

### 交互式模式 (默认)
```bash
# 运行脚本，进入交互式菜单
./cleanup_json_files.sh
```

**交互式菜单选项：**
1. **删除指定文件** - 输入文件编号选择性删除
2. **删除所有文件** - 一键删除所有JSON文件
3. **重新扫描文件** - 刷新文件列表
4. **退出** - 退出程序

### 命令行模式

#### 查看帮助
```bash
./cleanup_json_files.sh --help
```

#### 仅列出文件（不删除）
```bash
./cleanup_json_files.sh --list
```

#### 自动删除所有JSON文件
```bash
./cleanup_json_files.sh --auto
```

#### 删除指定模式的文件
```bash
# 删除网格状态文件
./cleanup_json_files.sh --pattern "grid_*.json"

# 删除性能数据文件
./cleanup_json_files.sh --pattern "performance_*.json"
```

#### 不创建备份直接删除
```bash
./cleanup_json_files.sh --auto --no-backup
```

## 📊 功能特性

### ✅ 安全特性
- **自动备份**: 删除前自动创建备份到 `backup_deleted_files/` 目录
- **确认提示**: 交互式确认避免误删
- **文件验证**: 检查文件存在性和权限
- **错误处理**: 详细的错误信息和恢复建议

### 📋 文件信息显示
- **文件大小**: 显示人类可读的文件大小
- **修改时间**: 显示最后修改时间
- **内容预览**: 显示JSON文件的关键信息
  - 网格状态：资金和利润信息
  - 订单状态：活跃订单数量
  - 性能数据：交易次数等

### 🎯 灵活选择
- **单个删除**: 选择特定文件删除
- **批量删除**: 支持范围选择 (如: 1,3,5 或 1-3)
- **模式匹配**: 使用通配符匹配文件
- **全部删除**: 一键清理所有JSON文件

## 📝 使用示例

### 示例1：交互式删除
```bash
$ ./cleanup_json_files.sh

🧹 JSON文件清理工具
========================
ℹ️  正在扫描JSON文件...

📁 找到以下 3 个JSON文件:
==============================================
1. grid_state.json
    📍 路径: ./grid_state.json
    📏 大小: 2.1K
    🕒 修改时间: 2024-01-15 14:30:25
    📋 内容: 网格状态 - 资金: 1000.0, 利润: 25.5

2. orders_state.json
    📍 路径: ./orders_state.json
    📏 大小: 1.5K
    🕒 修改时间: 2024-01-15 14:29:18
    📋 内容: 订单状态 - 活跃: 8, 买单: 5, 卖单: 3

3. dynamic_grid_params.json
    📍 路径: ./dynamic_grid_params.json
    📏 大小: 856B
    🕒 修改时间: 2024-01-15 14:25:10
    📋 内容: JSON文件

📋 操作选项:
1. 删除指定文件 (输入文件编号，用逗号分隔)
2. 删除所有文件
3. 重新扫描文件
4. 退出

请选择操作 (1-4): 1

请输入要删除的文件编号 (例如: 1,3,5 或 1-3): 1,2

⚠️  准备删除以下 2 个文件:
   - grid_state.json
   - orders_state.json

❓ 确认删除这些文件吗? (y/N): y

ℹ️  创建备份到: backup_deleted_files
✅ 备份: grid_state.json → 20240115_143045_grid_state.json
✅ 备份: orders_state.json → 20240115_143045_orders_state.json

ℹ️  开始删除文件...
✅ 已删除: grid_state.json
✅ 已删除: orders_state.json

📊 删除结果:
✅ 成功删除: 2 个文件
```

### 示例2：自动删除所有文件
```bash
$ ./cleanup_json_files.sh --auto

ℹ️  找到 3 个JSON文件
⚠️  准备删除以下 3 个文件:
   - grid_state.json
   - orders_state.json
   - dynamic_grid_params.json

ℹ️  创建备份到: backup_deleted_files
✅ 备份: grid_state.json → 20240115_143125_grid_state.json
✅ 备份: orders_state.json → 20240115_143125_orders_state.json
✅ 备份: dynamic_grid_params.json → 20240115_143125_dynamic_grid_params.json

ℹ️  开始删除文件...
✅ 已删除: grid_state.json
✅ 已删除: orders_state.json
✅ 已删除: dynamic_grid_params.json

📊 删除结果:
✅ 成功删除: 3 个文件
```

### 示例3：仅列出文件
```bash
$ ./cleanup_json_files.sh --list

📁 找到以下 2 个JSON文件:
==============================================
1. performance_20240115.json
    📍 路径: ./performance_20240115.json
    📏 大小: 3.2K
    🕒 修改时间: 2024-01-15 15:00:12
    📋 内容: 性能数据 - 交易次数: 45

2. trading_history_20240115.json
    📍 路径: ./trading_history_20240115.json
    📏 大小: 12.5K
    🕒 修改时间: 2024-01-15 15:00:15
    📋 内容: 交易历史 - 45 条记录
```

## 🔧 高级用法

### 删除特定类型的文件
```bash
# 只删除状态文件
./cleanup_json_files.sh --pattern "*_state.json"

# 只删除历史文件
./cleanup_json_files.sh --pattern "*_history_*.json"

# 只删除今天的文件
./cleanup_json_files.sh --pattern "*$(date +%Y%m%d)*.json"
```

### 批量操作
```bash
# 删除所有文件但不备份（谨慎使用）
./cleanup_json_files.sh --auto --no-backup

# 组合使用：先列出，再删除
./cleanup_json_files.sh --list
./cleanup_json_files.sh --pattern "grid_*.json" --auto
```

## 📁 备份管理

### 备份位置
- 备份目录：`backup_deleted_files/`
- 备份格式：`YYYYMMDD_HHMMSS_原文件名.json`

### 恢复文件
```bash
# 查看备份文件
ls -la backup_deleted_files/

# 恢复特定文件
cp backup_deleted_files/20240115_143045_grid_state.json grid_state.json
```

### 清理旧备份
```bash
# 删除7天前的备份
find backup_deleted_files/ -name "*.json" -mtime +7 -delete

# 删除所有备份
rm -rf backup_deleted_files/
```

## ⚠️ 注意事项

1. **数据安全**: 删除前会自动创建备份，但请确保重要数据已另外保存
2. **权限要求**: 需要对文件和目录有读写权限
3. **依赖工具**: Shell脚本可选依赖 `jq` 来显示JSON内容详情
4. **恢复能力**: 备份文件可用于恢复，但建议定期清理旧备份

## 🛠️ 故障排除

### 常见问题

**Q: 脚本没有执行权限**
```bash
chmod +x cleanup_json_files.sh
```

**Q: 找不到JSON文件**
- 确认当前目录是否正确
- 检查文件是否存在：`ls *.json`

**Q: 删除失败**
- 检查文件权限：`ls -la *.json`
- 确认文件未被其他程序占用

**Q: 备份目录创建失败**
- 检查当前目录写权限
- 手动创建：`mkdir backup_deleted_files`

### 获取帮助
```bash
# 查看脚本帮助
./cleanup_json_files.sh --help

# 查看详细信息
./cleanup_json_files.sh --list
```

---

## 🎯 总结

这些清理脚本提供了安全、灵活的JSON文件管理方案：

- ✅ **安全可靠**: 自动备份 + 确认提示
- ✅ **功能丰富**: 交互式 + 命令行模式
- ✅ **信息详细**: 文件大小、时间、内容预览
- ✅ **操作灵活**: 单选、多选、模式匹配
- ✅ **易于使用**: 直观的界面和清晰的提示

**推荐使用Shell脚本版本**，它提供了最佳的用户体验和功能完整性。 