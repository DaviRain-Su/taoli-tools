#!/bin/bash

# 网格交易JSON文件清理脚本
# 用于手动删除保存的状态文件和配置文件

set -e

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
PURPLE='\033[0;35m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# 配置
BACKUP_DIR="backup_deleted_files"
TIMESTAMP=$(date +"%Y%m%d_%H%M%S")

# 函数：打印带颜色的消息
print_info() {
    echo -e "${BLUE}ℹ️  $1${NC}"
}

print_success() {
    echo -e "${GREEN}✅ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}⚠️  $1${NC}"
}

print_error() {
    echo -e "${RED}❌ $1${NC}"
}

print_header() {
    echo -e "${PURPLE}$1${NC}"
}

# 函数：显示帮助信息
show_help() {
    echo "🧹 JSON文件清理工具"
    echo "===================="
    echo ""
    echo "用法:"
    echo "  $0 [选项]"
    echo ""
    echo "选项:"
    echo "  -h, --help          显示此帮助信息"
    echo "  -a, --auto          自动删除所有JSON文件"
    echo "  -n, --no-backup     不创建备份"
    echo "  -p, --pattern PATTERN 指定文件模式 (例如: 'grid_*.json')"
    echo "  -l, --list          仅列出文件，不删除"
    echo ""
    echo "示例:"
    echo "  $0                  # 交互式模式"
    echo "  $0 -a               # 自动删除所有JSON文件"
    echo "  $0 -p 'grid_*.json' # 删除匹配模式的文件"
    echo "  $0 -l               # 仅列出JSON文件"
}

# 函数：扫描JSON文件
scan_json_files() {
    local pattern="${1:-*.json}"
    
    # 清空文件列表
    > /tmp/json_files_list.txt
    
    # 扫描当前目录的JSON文件
    find . -maxdepth 1 -name "$pattern" -type f 2>/dev/null | sort >> /tmp/json_files_list.txt
    
    # 扫描子目录的JSON文件（可选）
    # find . -name "$pattern" -type f 2>/dev/null | sort >> /tmp/json_files_list.txt
    
    # 常见的网格交易JSON文件
    local common_files=(
        "grid_state.json"
        "orders_state.json"
        "dynamic_grid_params.json"
        "performance_*.json"
        "trading_history_*.json"
        "risk_events_*.json"
    )
    
    for file_pattern in "${common_files[@]}"; do
        find . -maxdepth 1 -name "$file_pattern" -type f 2>/dev/null >> /tmp/json_files_list.txt
    done
    
    # 去重并排序
    sort /tmp/json_files_list.txt | uniq > /tmp/json_files_unique.txt
    mv /tmp/json_files_unique.txt /tmp/json_files_list.txt
}

# 函数：显示文件信息
get_file_info() {
    local file="$1"
    local size=$(ls -lh "$file" | awk '{print $5}')
    local mtime=$(stat -f "%Sm" -t "%Y-%m-%d %H:%M:%S" "$file" 2>/dev/null || stat -c "%y" "$file" 2>/dev/null | cut -d'.' -f1)
    
    # 尝试获取JSON内容信息
    local content_info=""
    if command -v jq >/dev/null 2>&1; then
        if [[ "$file" == *"grid_state"* ]]; then
            local capital=$(jq -r '.total_capital // "N/A"' "$file" 2>/dev/null)
            local profit=$(jq -r '.realized_profit // "N/A"' "$file" 2>/dev/null)
            content_info="网格状态 - 资金: $capital, 利润: $profit"
        elif [[ "$file" == *"orders_state"* ]]; then
            local active=$(jq -r '.active_orders | length' "$file" 2>/dev/null)
            local buy=$(jq -r '.buy_orders | length' "$file" 2>/dev/null)
            local sell=$(jq -r '.sell_orders | length' "$file" 2>/dev/null)
            content_info="订单状态 - 活跃: $active, 买单: $buy, 卖单: $sell"
        elif [[ "$file" == *"performance"* ]]; then
            local trades=$(jq -r '.total_trades // "N/A"' "$file" 2>/dev/null)
            content_info="性能数据 - 交易次数: $trades"
        else
            content_info="JSON文件"
        fi
    else
        content_info="JSON文件 (安装jq可显示详细信息)"
    fi
    
    echo "    📍 路径: $file"
    echo "    📏 大小: $size"
    echo "    🕒 修改时间: $mtime"
    echo "    📋 内容: $content_info"
}

# 函数：显示文件列表
display_files() {
    local file_count=$(wc -l < /tmp/json_files_list.txt)
    
    if [ "$file_count" -eq 0 ]; then
        print_error "未找到任何JSON文件"
        return 1
    fi
    
    print_header "\n📁 找到以下 $file_count 个JSON文件:"
    echo "=============================================="
    
    local index=1
    while IFS= read -r file; do
        if [ -f "$file" ]; then
            echo -e "${CYAN}$index. $(basename "$file")${NC}"
            get_file_info "$file"
            echo ""
            ((index++))
        fi
    done < /tmp/json_files_list.txt
    
    return 0
}

# 函数：创建备份
create_backup() {
    local files_to_backup=("$@")
    
    if [ ${#files_to_backup[@]} -eq 0 ]; then
        return 0
    fi
    
    print_info "创建备份到: $BACKUP_DIR"
    mkdir -p "$BACKUP_DIR"
    
    for file in "${files_to_backup[@]}"; do
        if [ -f "$file" ]; then
            local backup_name="${TIMESTAMP}_$(basename "$file")"
            cp "$file" "$BACKUP_DIR/$backup_name"
            print_success "备份: $(basename "$file") → $backup_name"
        fi
    done
}

# 函数：删除文件
delete_files() {
    local indices=("$@")
    local files_to_delete=()
    
    # 读取文件列表到数组
    local file_array=()
    while IFS= read -r file; do
        file_array+=("$file")
    done < /tmp/json_files_list.txt
    
    # 根据索引收集要删除的文件
    for index in "${indices[@]}"; do
        if [ "$index" -ge 1 ] && [ "$index" -le "${#file_array[@]}" ]; then
            local file="${file_array[$((index-1))]}"
            if [ -f "$file" ]; then
                files_to_delete+=("$file")
            fi
        else
            print_error "无效的文件编号: $index"
        fi
    done
    
    if [ ${#files_to_delete[@]} -eq 0 ]; then
        print_error "没有有效的文件可删除"
        return 1
    fi
    
    # 显示将要删除的文件
    print_warning "准备删除以下 ${#files_to_delete[@]} 个文件:"
    for file in "${files_to_delete[@]}"; do
        echo "   - $(basename "$file")"
    done
    
    # 确认删除
    if [ "$AUTO_MODE" != "true" ]; then
        echo ""
        read -p "❓ 确认删除这些文件吗? (y/N): " -r
        if [[ ! $REPLY =~ ^[Yy]$ ]]; then
            print_error "删除操作已取消"
            return 1
        fi
    fi
    
    # 创建备份
    if [ "$NO_BACKUP" != "true" ]; then
        create_backup "${files_to_delete[@]}"
    fi
    
    # 执行删除
    print_info "开始删除文件..."
    local deleted_count=0
    local failed_files=()
    
    for file in "${files_to_delete[@]}"; do
        if rm "$file" 2>/dev/null; then
            print_success "已删除: $(basename "$file")"
            ((deleted_count++))
        else
            print_error "删除失败: $(basename "$file")"
            failed_files+=("$(basename "$file")")
        fi
    done
    
    # 删除结果
    echo ""
    print_header "📊 删除结果:"
    print_success "成功删除: $deleted_count 个文件"
    if [ ${#failed_files[@]} -gt 0 ]; then
        print_error "删除失败: ${#failed_files[@]} 个文件"
        for failed_file in "${failed_files[@]}"; do
            echo "      - $failed_file"
        done
    fi
    
    return 0
}

# 函数：解析索引范围
parse_indices() {
    local input="$1"
    local indices=()
    
    IFS=',' read -ra PARTS <<< "$input"
    for part in "${PARTS[@]}"; do
        part=$(echo "$part" | xargs) # 去除空格
        if [[ "$part" =~ ^[0-9]+$ ]]; then
            # 单个数字
            indices+=("$part")
        elif [[ "$part" =~ ^[0-9]+-[0-9]+$ ]]; then
            # 范围 (例如: 1-3)
            local start=$(echo "$part" | cut -d'-' -f1)
            local end=$(echo "$part" | cut -d'-' -f2)
            for ((i=start; i<=end; i++)); do
                indices+=("$i")
            done
        else
            print_error "无效的索引格式: $part"
            return 1
        fi
    done
    
    # 去重并排序
    printf '%s\n' "${indices[@]}" | sort -nu
}

# 函数：交互式模式
interactive_mode() {
    print_header "🧹 JSON文件清理工具"
    echo "========================"
    
    while true; do
        # 扫描文件
        print_info "正在扫描JSON文件..."
        scan_json_files
        
        if ! display_files; then
            echo ""
            read -p "按回车键退出..." -r
            break
        fi
        
        echo ""
        print_header "📋 操作选项:"
        echo "1. 删除指定文件 (输入文件编号，用逗号分隔)"
        echo "2. 删除所有文件"
        echo "3. 重新扫描文件"
        echo "4. 退出"
        echo ""
        
        read -p "请选择操作 (1-4): " -r choice
        
        case $choice in
            1)
                echo ""
                read -p "请输入要删除的文件编号 (例如: 1,3,5 或 1-3): " -r indices_input
                if [ -n "$indices_input" ]; then
                    local indices_array=($(parse_indices "$indices_input"))
                    if [ ${#indices_array[@]} -gt 0 ]; then
                        delete_files "${indices_array[@]}"
                    fi
                else
                    print_error "未输入任何编号"
                fi
                ;;
            2)
                local file_count=$(wc -l < /tmp/json_files_list.txt)
                if [ "$file_count" -gt 0 ]; then
                    local all_indices=($(seq 1 "$file_count"))
                    delete_files "${all_indices[@]}"
                else
                    print_error "没有文件可删除"
                fi
                ;;
            3)
                continue
                ;;
            4)
                print_info "退出程序"
                break
                ;;
            *)
                print_error "无效选择，请输入 1-4"
                ;;
        esac
        
        echo ""
        read -p "按回车键继续..." -r
        echo ""
    done
}

# 主函数
main() {
    local AUTO_MODE=false
    local NO_BACKUP=false
    local LIST_ONLY=false
    local PATTERN="*.json"
    
    # 解析命令行参数
    while [[ $# -gt 0 ]]; do
        case $1 in
            -h|--help)
                show_help
                exit 0
                ;;
            -a|--auto)
                AUTO_MODE=true
                shift
                ;;
            -n|--no-backup)
                NO_BACKUP=true
                shift
                ;;
            -l|--list)
                LIST_ONLY=true
                shift
                ;;
            -p|--pattern)
                PATTERN="$2"
                shift 2
                ;;
            *)
                print_error "未知选项: $1"
                show_help
                exit 1
                ;;
        esac
    done
    
    # 导出变量供其他函数使用
    export AUTO_MODE NO_BACKUP
    
    # 扫描文件
    scan_json_files "$PATTERN"
    
    if [ "$LIST_ONLY" = true ]; then
        # 仅列出文件
        display_files
        exit 0
    fi
    
    if [ "$AUTO_MODE" = true ]; then
        # 自动模式
        local file_count=$(wc -l < /tmp/json_files_list.txt)
        if [ "$file_count" -gt 0 ]; then
            print_info "找到 $file_count 个JSON文件"
            local all_indices=($(seq 1 "$file_count"))
            delete_files "${all_indices[@]}"
        else
            print_error "未找到任何JSON文件"
        fi
    else
        # 交互模式
        interactive_mode
    fi
    
    # 清理临时文件
    rm -f /tmp/json_files_list.txt
}

# 清理函数
cleanup() {
    rm -f /tmp/json_files_list.txt
}

# 设置退出时清理
trap cleanup EXIT

# 运行主函数
main "$@" 