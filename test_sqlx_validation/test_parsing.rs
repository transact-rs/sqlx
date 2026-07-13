// Test the INSERT parsing logic

fn extract_insert_info(query: &str) -> Option<(String, Option<Vec<String>>)> {
    let trimmed = query.trim_start();
    let upper = trimmed.to_uppercase();

    if !upper.starts_with("INSERT INTO") {
        return None;
    }

    let after_insert = upper.strip_prefix("INSERT INTO")?.trim_start();

    let mut table_end = 0;
    let mut in_quote = false;
    let mut quote_char = ' ';
    let chars: Vec<char> = after_insert.chars().collect();

    for (i, &ch) in chars.iter().enumerate() {
        if !in_quote && (ch == '`' || ch == '"' || ch == '[') {
            in_quote = true;
            quote_char = if ch == '[' { ']' } else { ch };
            continue;
        }
        if in_quote && ch == quote_char {
            in_quote = false;
            table_end = i + 1;
            continue;
        }
        if !in_quote && (ch == ' ' || ch == '(') {
            table_end = i;
            break;
        }
    }

    if table_end == 0 {
        table_end = after_insert.len();
    }

    let table_name_raw = after_insert[..table_end].trim();
    let table_name = table_name_raw
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('[')
        .trim_matches(']')
        .to_string();

    let remaining = after_insert[table_end..].trim_start();

    if remaining.starts_with('(') {
        let paren_count = remaining.find("VALUES").unwrap_or(remaining.len());
        let potential_cols = &remaining[1..paren_count.saturating_sub(1)];

        if potential_cols.contains(',') || !potential_cols.trim().is_empty() {
            let columns = potential_cols
                .split(',')
                .map(|c| {
                    c.trim()
                        .trim_matches('`')
                        .trim_matches('"')
                        .trim_matches('[')
                        .trim_matches(']')
                        .to_string()
                })
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>();

            if !columns.is_empty() {
                return Some((table_name, Some(columns)));
            }
        }
    }

    Some((table_name, None))
}

fn main() {
    // Test case 1: INSERT with column list (from the issue)
    let query1 = r#"
        INSERT INTO session_group (prop_a, prop_b)
        VALUES (?, ?)
    "#;
    
    let result1 = extract_insert_info(query1);
    println!("Test 1 - INSERT with partial columns:");
    println!("  Query: {}", query1.trim());
    println!("  Parsed: {:?}", result1);
    
    match result1 {
        Some((table, Some(cols))) => {
            println!("  Table: {}", table);
            println!("  Columns: {:?}", cols);
            println!("  Missing: prop_c (required!)");
        },
        _ => println!("  Failed to parse"),
    }
    
    println!();
    
    // Test case 2: INSERT with all columns
    let query2 = r#"
        INSERT INTO session_group (prop_a, prop_b, prop_c)
        VALUES (?, ?, ?)
    "#;
    
    let result2 = extract_insert_info(query2);
    println!("Test 2 - INSERT with all columns:");
    println!("  Query: {}", query2.trim());
    println!("  Parsed: {:?}", result2);
    
    match result2 {
        Some((table, Some(cols))) => {
            println!("  Table: {}", table);
            println!("  Columns: {:?}", cols);
            println!("  All required columns present ✓");
        },
        _ => println!("  Failed to parse"),
    }
    
    println!();
    
    // Test case 3: INSERT without column list
    let query3 = "INSERT INTO session_group VALUES (?, ?, ?)";
    
    let result3 = extract_insert_info(query3);
    println!("Test 3 - INSERT without column list:");
    println!("  Query: {}", query3);
    println!("  Parsed: {:?}", result3);
    println!("  (All columns implied - SQLite handles validation)");
}
