#!/bin/bash
# Check Mermaid syntax in markdown files
# Usage: bash scripts/check-mermaid.sh [file.md]

set -e

check_file() {
    local file="$1"
    echo "Checking $file..."

    # Extract and validate each mermaid block
    local in_block=false
    local block_num=0
    local temp_file="$TEMP/mermaid_check_$$.mmd"
    local out_file="$TEMP/mermaid_out_$$.svg"

    while IFS= read -r line; do
        if [[ "$line" =~ ^\`\`\`mermaid$ ]]; then
            in_block=true
            block_num=$((block_num + 1))
            : > "$temp_file"
            continue
        fi

        if [[ "$line" =~ ^\`\`\`$ ]] && $in_block; then
            in_block=false
            if [ -s "$temp_file" ]; then
                echo "  Block $block_num:"
                mmdc -i "$temp_file" -o "$out_file" 2>&1 | head -10 || true
                rm -f "$out_file"
            fi
            continue
        fi

        if $in_block; then
            echo "$line" >> "$temp_file"
        fi
    done < "$file"

    rm -f "$temp_file"
}

if [ -n "$1" ]; then
    check_file "$1"
else
    # Check all markdown files
    find . -name "*.md" -exec grep -l '```mermaid' {} \; 2>/dev/null | while read -r file; do
        check_file "$file"
    done
fi
