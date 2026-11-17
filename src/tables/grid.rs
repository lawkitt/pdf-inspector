//! Column/row boundary detection and cell assignment for heuristic tables.

use crate::types::TextItem;

use super::{Table, TableDetectionMode};

pub(crate) fn find_column_boundaries(
    items: &[(usize, &TextItem)],
    mode: TableDetectionMode,
) -> Vec<f32> {
    let mut x_positions: Vec<f32> = items.iter().map(|(_, i)| i.x).collect();
    x_positions.sort_by(|a, b| a.total_cmp(b));

    if x_positions.is_empty() {
        return vec![];
    }

    // For dense, narrow-column tables (e.g. train schedules with 24 cols at
    // 26pt spacing), the old avg_gap approach over-clusters because avg_gap is
    // dominated by the many items *within* each column.  Use a gap-histogram
    // on consecutive position gaps to detect when columns are densely packed,
    // and only then lower the threshold below 25pt.
    let x_range = x_positions.last().unwrap() - x_positions.first().unwrap();
    let avg_gap = if x_positions.len() > 1 {
        x_range / (x_positions.len() - 1) as f32
    } else {
        60.0
    };

    // Default: original avg_gap approach, center-based clustering
    let mut cluster_threshold = avg_gap.clamp(25.0, 50.0);
    let mut use_edge_clustering = false;

    // Analyze the distribution of non-trivial consecutive gaps to detect
    // a bimodal pattern (small within-column gaps vs large between-column gaps).
    // When detected, switch to edge-based clustering with the lower threshold
    // to correctly separate densely-packed columns without over-splitting
    // wide columns (edge-based avoids the center-drift problem).
    let mut consec_gaps: Vec<f32> = x_positions
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|&g| g > 0.1) // skip near-duplicate positions
        .collect();

    if consec_gaps.len() > 2 {
        consec_gaps.sort_by(|a, b| a.total_cmp(b));
        // Find the biggest jump in the sorted gap sequence — natural break
        // between within-column jitter and between-column spacing.
        // Require at least 3 values on each side to avoid outlier-dominated
        // splits (e.g. a single large page-margin gap).
        let mut best_split = consec_gaps.len() / 2;
        let mut best_jump = 0.0f32;
        let min_side = 3.min(consec_gaps.len() / 2);
        for i in 0..consec_gaps.len().saturating_sub(1) {
            let left_count = i + 1;
            let right_count = consec_gaps.len() - i - 1;
            if left_count < min_side || right_count < min_side {
                continue;
            }
            let jump = consec_gaps[i + 1] - consec_gaps[i];
            if jump > best_jump {
                best_jump = jump;
                best_split = i;
            }
        }
        let threshold = (consec_gaps[best_split]
            + consec_gaps[(best_split + 1).min(consec_gaps.len() - 1)])
            / 2.0;
        // Override for tables with a clear bimodal gap pattern:
        // - Dense tables (500+ items, e.g. 24-column train schedule): use
        //   edge-based clustering with the detected threshold.
        // - Smaller tables with a strong bimodal signal (jump > 10pt):
        //   lower the threshold but keep center-based clustering to avoid
        //   over-splitting.
        if threshold < 15.0 && best_jump > 2.0 && x_positions.len() > 500 {
            cluster_threshold = threshold.clamp(8.0, 25.0);
            use_edge_clustering = true;
        } else if best_jump > 10.0 && threshold < cluster_threshold {
            // Strong bimodal signal even with fewer items — the gap between
            // within-column jitter and between-column spacing is unambiguous.
            cluster_threshold = threshold.max(8.0);
        }
    }

    // Track cluster membership: for each cluster, store the list of x positions
    let mut cluster_xs: Vec<Vec<f32>> = vec![vec![x_positions[0]]];

    for &x in &x_positions[1..] {
        let last_cluster = cluster_xs.last().unwrap();
        // For dense columns (gap-histogram triggered), use edge-based clustering:
        // compare with the last item to avoid center-drift that merges adjacent
        // narrow columns.  For normal tables, use center-based (original behavior).
        let reference = if use_edge_clustering {
            *last_cluster.last().unwrap()
        } else {
            last_cluster.iter().sum::<f32>() / last_cluster.len() as f32
        };

        if x - reference > cluster_threshold {
            cluster_xs.push(vec![x]);
        } else {
            cluster_xs.last_mut().unwrap().push(x);
        }
    }

    // Numeric column merge pass: when a sparse cluster (few items, typically
    // header text) is adjacent to a dense numeric cluster and within 1.5×
    // threshold, merge them. This fixes tables where multi-line wrapped
    // headers have slightly different X positions than the data columns,
    // causing the header and data to split into separate clusters.
    let columns_before_merge = cluster_xs.len();
    if columns_before_merge >= 3 {
        cluster_xs = merge_numeric_adjacent_clusters(cluster_xs, items, cluster_threshold);
    }

    let columns: Vec<f32> = cluster_xs
        .iter()
        .map(|xs| xs.iter().sum::<f32>() / xs.len() as f32)
        .collect();

    // Filter columns - each should have multiple items
    let min_items_per_col = (items.len() / columns.len().max(1) / 4).max(2);
    let columns: Vec<f32> = columns
        .into_iter()
        .filter(|&col_x| {
            items
                .iter()
                .filter(|(_, i)| (i.x - col_x).abs() < cluster_threshold)
                .count()
                >= min_items_per_col
        })
        .collect();

    log::debug!(
        "  find_column_boundaries: {} columns (merged from {}), threshold={:.1}, {} items",
        columns.len(),
        columns_before_merge,
        cluster_threshold,
        items.len()
    );

    // Anti-paragraph safeguard for BodyFont mode:
    // Paragraphs concentrate items at the left margin; tables distribute evenly.
    // Reject if any single column has >60% of all items.
    if mode == TableDetectionMode::BodyFont {
        let total_items = items.len();
        for &col_x in &columns {
            let count = items
                .iter()
                .filter(|(_, i)| (i.x - col_x).abs() < cluster_threshold)
                .count();
            if count as f32 / total_items as f32 > 0.60 {
                return vec![];
            }
        }
    }

    columns
}

/// Check if a text string looks like a number (digits, decimals, sign, comma).
fn is_numeric_text(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    // Match patterns like: 8.23, -1.05, 9.99, 7.12, 100, 3,456.78, +5%, ---
    // But NOT: BIO, Department, Core Courses
    s.chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == ',' || c == '-' || c == '+' || c == '%')
        && s.chars().any(|c| c.is_ascii_digit())
}

/// Merge adjacent X-position clusters when one is a sparse header cluster
/// and the other is a dense numeric data cluster. This prevents multi-line
/// wrapped headers from splitting a logical column into two clusters.
fn merge_numeric_adjacent_clusters(
    mut clusters: Vec<Vec<f32>>,
    items: &[(usize, &TextItem)],
    threshold: f32,
) -> Vec<Vec<f32>> {
    // For each cluster, compute: center, item count, numeric fraction
    struct ClusterInfo {
        center: f32,
        count: usize,
        numeric_frac: f32,
    }

    let compute_info = |xs: &[f32]| -> ClusterInfo {
        let center = xs.iter().sum::<f32>() / xs.len() as f32;
        // Count items and numeric fraction for items near this cluster center
        let mut total = 0;
        let mut numeric = 0;
        for (_, item) in items {
            if (item.x - center).abs() < threshold {
                total += 1;
                if is_numeric_text(&item.text) {
                    numeric += 1;
                }
            }
        }
        ClusterInfo {
            center,
            count: total,
            numeric_frac: if total > 0 {
                numeric as f32 / total as f32
            } else {
                0.0
            },
        }
    };

    // Merge distance: allow merging clusters that are slightly beyond the
    // original threshold. Use 1.5× threshold to catch header-vs-data splits.
    let merge_dist = threshold * 1.5;

    // Iterate and merge adjacent pairs. Use a simple left-to-right scan.
    let mut merged = true;
    while merged {
        merged = false;
        let mut i = 0;
        while i + 1 < clusters.len() {
            let info_a = compute_info(&clusters[i]);
            let info_b = compute_info(&clusters[i + 1]);
            let dist = (info_b.center - info_a.center).abs();

            if dist > merge_dist {
                i += 1;
                continue;
            }

            // Determine if one cluster is sparse (header) and the other
            // is dense and numeric (data). A cluster is "sparse" if it has
            // significantly fewer items than the other.
            let (sparse, dense) = if info_a.count < info_b.count {
                (&info_a, &info_b)
            } else {
                (&info_b, &info_a)
            };

            // Merge if the dense cluster is predominantly numeric (>50%)
            // and the sparse cluster has at most 1/3 the items of the dense one.
            let should_merge =
                dense.numeric_frac > 0.50 && sparse.count <= dense.count / 2 && sparse.count <= 5;

            if should_merge {
                log::debug!(
                    "  merging column clusters: center {:.1} ({} items, {:.0}% numeric) + {:.1} ({} items, {:.0}% numeric), dist={:.1}",
                    info_a.center,
                    info_a.count,
                    info_a.numeric_frac * 100.0,
                    info_b.center,
                    info_b.count,
                    info_b.numeric_frac * 100.0,
                    dist,
                );
                // Merge cluster i+1 into cluster i
                let next = clusters.remove(i + 1);
                clusters[i].extend(next);
                merged = true;
                // Don't increment i — check if the merged cluster can merge further
            } else {
                i += 1;
            }
        }
    }

    clusters
}

/// Find row boundaries by clustering Y positions
pub(crate) fn find_row_boundaries(items: &[(usize, &TextItem)]) -> Vec<f32> {
    let mut y_positions: Vec<f32> = items.iter().map(|(_, i)| i.y).collect();
    y_positions.sort_by(|a, b| b.total_cmp(a)); // Descending

    if y_positions.is_empty() {
        return vec![];
    }

    // Cluster Y positions - items within a fraction of the median font size are same row.
    // Using 0.8× median font keeps the threshold between intra-row gaps (~0pt) and
    // inter-row gaps (≥1× font size), preventing row merging in uniform-spaced PDFs.
    let cluster_threshold = {
        let mut font_sizes: Vec<f32> = items.iter().map(|(_, i)| i.font_size).collect();
        font_sizes.sort_by(|a, b| a.total_cmp(b));
        let median_font = font_sizes[font_sizes.len() / 2];
        (median_font * 0.8).max(4.0)
    };
    let mut rows = Vec::new();
    let mut cluster_items: Vec<f32> = vec![y_positions[0]];

    for &y in &y_positions[1..] {
        let cluster_center = cluster_items.iter().sum::<f32>() / cluster_items.len() as f32;

        if cluster_center - y >= cluster_threshold {
            // End current cluster (note: Y is descending)
            rows.push(cluster_center);
            cluster_items = vec![y];
        } else {
            cluster_items.push(y);
        }
    }

    if !cluster_items.is_empty() {
        rows.push(cluster_items.iter().sum::<f32>() / cluster_items.len() as f32);
    }

    rows
}

/// Whether `below`, the next line of a table cell, reads as a continuation
/// of `above`: it starts in lowercase, or `above` breaks off on a hyphen,
/// dash, comma or slash, or on a lowercase word with no closing punctuation.
pub(crate) fn continues_cell_text(above: &str, below: &str) -> bool {
    let above = above.trim_end();
    let below = below.trim_start();
    if above.is_empty() || below.is_empty() {
        return false;
    }
    if below.chars().next().is_some_and(char::is_lowercase) {
        return true;
    }
    if above.ends_with(['-', '\u{2010}', '\u{2013}', '\u{2014}', ',', '/', '&']) {
        return true;
    }
    above.split_whitespace().last().is_some_and(|word| {
        word.chars().all(char::is_alphabetic) && word.chars().next().is_some_and(char::is_lowercase)
    })
}

/// Which rows of a borderless table are only the next line of the row
/// above, its cells wrapped (upstream #315). Row clustering gives each
/// baseline its own row, so a wrapped row came out as two. A row is such a
/// line when the table's baselines fall into two spacings, line pitch and a
/// looser row pitch, and the row sits at line pitch under the row above,
/// fills fewer cells than it, only cells the row above fills too, has no
/// cell that is a bare number, and at least half of its cells continue the text above
/// (`continues_cell_text`). `rows` are the rows' baselines, top to bottom.
pub(crate) fn wrapped_line_rows(rows: &[f32], cells: &[Vec<String>]) -> Vec<bool> {
    let mut wrapped = vec![false; rows.len()];
    if rows.len() < 3 || cells.len() != rows.len() {
        return wrapped;
    }
    let gaps: Vec<f32> = rows
        .windows(2)
        .map(|pair| (pair[0] - pair[1]).abs())
        .collect();
    let line_pitch = gaps.iter().copied().fold(f32::INFINITY, f32::min);
    let row_pitch = gaps.iter().copied().fold(0.0_f32, f32::max);
    if !line_pitch.is_finite() || line_pitch <= 0.0 || row_pitch < line_pitch * 1.3 {
        return wrapped;
    }
    let filled = |cell: &String| !cell.trim().is_empty();
    let numeric = |cell: &str| {
        cell.chars().any(|ch| ch.is_ascii_digit())
            && cell.chars().all(|ch| {
                ch.is_ascii_digit()
                    || ch.is_whitespace()
                    || matches!(ch, '.' | ',' | '-' | '%' | '$')
            })
    };
    for row in 1..rows.len() {
        if gaps[row - 1] > line_pitch * 1.15 {
            continue;
        }
        let (above, below) = (&cells[row - 1], &cells[row]);
        let lines: Vec<usize> = (0..below.len())
            .filter(|&col| filled(&below[col]))
            .collect();
        if lines.is_empty()
            || lines.len() >= above.iter().filter(|cell| filled(cell)).count()
            || lines.iter().any(|&col| !above.get(col).is_some_and(filled))
            || lines.iter().any(|&col| numeric(below[col].trim()))
        {
            continue;
        }
        let continuing = lines
            .iter()
            .filter(|&&col| continues_cell_text(&above[col], &below[col]))
            .count();
        wrapped[row] = continuing * 2 >= lines.len();
    }
    wrapped
}

/// Folds each row flagged in `wrapped` into the row above it with `merge`.
pub(crate) fn fold_wrapped_rows<T>(
    rows: Vec<T>,
    wrapped: &[bool],
    mut merge: impl FnMut(&mut T, T),
) -> Vec<T> {
    let mut folded: Vec<T> = Vec::with_capacity(rows.len());
    for (index, row) in rows.into_iter().enumerate() {
        match folded.last_mut() {
            Some(previous) if wrapped.get(index).copied().unwrap_or(false) => merge(previous, row),
            _ => folded.push(row),
        }
    }
    folded
}

/// Joins a wrapped line's cells onto the row above, cell by cell.
pub(crate) fn join_wrapped_cells(above: &mut [String], below: Vec<String>) {
    for (cell, line) in above.iter_mut().zip(below) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !cell.is_empty() {
            cell.push(' ');
        }
        cell.push_str(line);
    }
}

/// Find which column index an X position belongs to
pub(crate) fn find_column_index(columns: &[f32], x: f32) -> Option<usize> {
    // Calculate adaptive threshold based on column spacing
    let threshold = if columns.len() >= 2 {
        let min_gap = columns
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(f32::INFINITY, f32::min);
        (min_gap / 2.0).clamp(25.0, 50.0)
    } else {
        50.0
    };

    columns
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (x - *a)
                .abs()
                .partial_cmp(&(x - *b).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .filter(|(_, col_x)| (x - *col_x).abs() < threshold)
        .map(|(idx, _)| idx)
}

/// Find which row index a Y position belongs to
pub(crate) fn find_row_index(rows: &[f32], y: f32) -> Option<usize> {
    let threshold = 15.0;
    rows.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (y - *a)
                .abs()
                .partial_cmp(&(y - *b).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .filter(|(_, row_y)| (y - *row_y).abs() < threshold)
        .map(|(idx, _)| idx)
}

/// Recover a header row for small-font tables by looking at body-font items
/// just above the table's first row.
///
/// PDF tables often have header rows at the body font size while data rows use
/// a smaller font. Pass 1 (SmallFont) excludes the header because of the
/// font-size filter. This function looks upward from the table's first row for
/// body-font items that align with the table's columns, and prepends them.
pub(crate) fn recover_header_row(
    table: &mut Table,
    all_items: &[TextItem],
    small_font_threshold: f32,
) {
    if table.rows.is_empty() || table.columns.is_empty() {
        return;
    }

    let first_row_y = table.rows[0]; // highest Y (rows are descending)

    // Compute typical row spacing for gap threshold
    let row_gap_limit = if table.rows.len() >= 2 {
        let avg_spacing =
            (table.rows[0] - table.rows[table.rows.len() - 1]) / (table.rows.len() - 1) as f32;
        // Allow up to 2x average row spacing for the header gap
        (avg_spacing * 2.0).clamp(10.0, 40.0)
    } else {
        30.0
    };

    // Find body-font items just above the first row
    let header_candidates: Vec<(usize, &TextItem)> = all_items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            !item.is_strikeout
                && item.font_size > small_font_threshold
                && item.y > first_row_y
                && item.y <= first_row_y + row_gap_limit
        })
        .collect();

    if header_candidates.is_empty() {
        return;
    }

    // Group header candidates by Y (cluster within 5pt)
    let mut header_y_groups: Vec<(f32, Vec<(usize, &TextItem)>)> = Vec::new();
    let mut sorted_candidates = header_candidates;
    sorted_candidates.sort_by(|a, b| {
        b.1.y
            .partial_cmp(&a.1.y)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for (idx, item) in &sorted_candidates {
        let found = header_y_groups
            .iter_mut()
            .find(|(y, _)| (item.y - *y).abs() < 5.0);
        if let Some((_, group)) = found {
            group.push((*idx, item));
        } else {
            header_y_groups.push((item.y, vec![(*idx, item)]));
        }
    }

    // Take the row closest to the table (lowest Y above first_row_y)
    // header_y_groups is sorted by descending Y, so take the last one
    let (header_y, header_items) = header_y_groups.last().unwrap();

    // Map header items to table columns
    let num_cols = table.columns.len();
    let mut header_cells: Vec<String> = vec![String::new(); num_cols];
    let mut mapped_count = 0;
    let mut header_indices = Vec::new();

    for (idx, item) in header_items {
        if let Some(col) = find_column_index(&table.columns, item.x) {
            let text = item.text.trim();
            if !text.is_empty() {
                if !header_cells[col].is_empty() {
                    header_cells[col].push(' ');
                }
                header_cells[col].push_str(text);
                mapped_count += 1;
                header_indices.push(*idx);
            }
        }
    }

    // Require at least 2 columns populated to look like a real header row
    let populated = header_cells.iter().filter(|c| !c.is_empty()).count();
    if populated < 2 || mapped_count < 2 {
        return;
    }

    // Prepend header row to the table
    table.rows.insert(0, *header_y);
    table.cells.insert(0, header_cells);
    if !table.layout_rows.is_empty() {
        table.layout_rows.insert(0, false);
    }
    table.item_indices.extend(header_indices);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::TableKind;
    use crate::types::ItemType;

    fn make_item(text: &str, x: f32, y: f32, font_size: f32) -> TextItem {
        TextItem {
            text: text.to_string(),
            x,
            y,
            width: text.len() as f32 * font_size * 0.5,
            height: font_size,
            font: "TestFont".to_string(),
            font_tag: String::new(),
            legacy_symbol_rewrite: false,
            font_size,
            page: 1,
            is_bold: false,
            is_italic: false,
            font_weight: None,
            bold_source: None,
            fixed_pitch: None,
            fill_color: None,
            stroke_color: None,
            render_mode: None,
            is_underline: false,
            is_strikeout: false,
            layout_strikeout: false,
            rotation: 0.0,
            advance_known: true,
            item_type: ItemType::Text,
            mcid: None,
            baseline_shift: 0.0,
        }
    }

    // --- find_column_index ---

    #[test]
    fn test_find_column_index_exact_match() {
        let columns = vec![100.0, 200.0, 300.0];
        assert_eq!(find_column_index(&columns, 100.0), Some(0));
        assert_eq!(find_column_index(&columns, 200.0), Some(1));
        assert_eq!(find_column_index(&columns, 300.0), Some(2));
    }

    #[test]
    fn test_find_column_index_closest_within_threshold() {
        let columns = vec![100.0, 200.0, 300.0];
        assert_eq!(find_column_index(&columns, 105.0), Some(0));
        assert_eq!(find_column_index(&columns, 195.0), Some(1));
    }

    #[test]
    fn test_find_column_index_outside_threshold() {
        let columns = vec![100.0, 200.0, 300.0];
        // Threshold is clamped to min 25, max 50 based on min gap / 2
        // Min gap = 100, threshold = clamp(50, 25, 50) = 50
        assert_eq!(find_column_index(&columns, 500.0), None);
    }

    #[test]
    fn test_find_column_index_single_column() {
        let columns = vec![150.0];
        // Single column → threshold = 50.0
        assert_eq!(find_column_index(&columns, 150.0), Some(0));
        assert_eq!(find_column_index(&columns, 170.0), Some(0));
    }

    #[test]
    fn test_find_column_index_empty_columns() {
        let columns: Vec<f32> = vec![];
        assert_eq!(find_column_index(&columns, 100.0), None);
    }

    // --- find_row_index ---

    #[test]
    fn test_find_row_index_exact_match() {
        let rows = vec![500.0, 480.0, 460.0];
        assert_eq!(find_row_index(&rows, 500.0), Some(0));
        assert_eq!(find_row_index(&rows, 480.0), Some(1));
    }

    #[test]
    fn test_find_row_index_within_threshold() {
        let rows = vec![500.0, 480.0, 460.0];
        assert_eq!(find_row_index(&rows, 505.0), Some(0));
        assert_eq!(find_row_index(&rows, 475.0), Some(1));
    }

    #[test]
    fn test_find_row_index_outside_threshold() {
        let rows = vec![500.0, 480.0, 460.0];
        // threshold is 15.0
        assert_eq!(find_row_index(&rows, 400.0), None);
    }

    #[test]
    fn test_find_row_index_single_row() {
        let rows = vec![500.0];
        assert_eq!(find_row_index(&rows, 500.0), Some(0));
        assert_eq!(find_row_index(&rows, 510.0), Some(0));
    }

    // --- find_column_boundaries ---

    #[test]
    fn test_find_column_boundaries_empty() {
        let items: Vec<(usize, &TextItem)> = vec![];
        assert_eq!(
            find_column_boundaries(&items, TableDetectionMode::SmallFont),
            Vec::<f32>::new()
        );
    }

    #[test]
    fn test_find_column_boundaries_two_clusters() {
        // Items at x=100 and x=200 with enough repetition
        let items_data: Vec<TextItem> = (0..10)
            .map(|i| {
                let x = if i % 2 == 0 { 100.0 } else { 200.0 };
                make_item("Cell", x, 500.0 - (i as f32 * 20.0), 10.0)
            })
            .collect();
        let items: Vec<(usize, &TextItem)> = items_data.iter().enumerate().collect();
        let cols = find_column_boundaries(&items, TableDetectionMode::SmallFont);
        assert_eq!(cols.len(), 2);
    }

    #[test]
    fn test_find_column_boundaries_single_item() {
        let item = make_item("Solo", 100.0, 500.0, 10.0);
        let items: Vec<(usize, &TextItem)> = vec![(0, &item)];
        // Single item won't pass the min_items_per_col filter (needs >=2)
        let cols = find_column_boundaries(&items, TableDetectionMode::SmallFont);
        assert!(cols.is_empty());
    }

    #[test]
    fn test_find_column_boundaries_body_font_paragraph_rejection() {
        // All items at same X → >60% in one column → rejected in BodyFont mode
        let items_data: Vec<TextItem> = (0..10)
            .map(|i| make_item("Text", 100.0, 500.0 - (i as f32 * 20.0), 10.0))
            .collect();
        let items: Vec<(usize, &TextItem)> = items_data.iter().enumerate().collect();
        let cols = find_column_boundaries(&items, TableDetectionMode::BodyFont);
        assert!(cols.is_empty());
    }

    #[test]
    fn test_find_column_boundaries_min_items_filter() {
        // Create 10 items at x=100 and 1 item at x=300
        // The single outlier should be filtered out
        let mut items_data: Vec<TextItem> = (0..10)
            .map(|i| make_item("Cell", 100.0, 500.0 - (i as f32 * 20.0), 10.0))
            .collect();
        items_data.push(make_item("Lone", 300.0, 500.0, 10.0));
        let items: Vec<(usize, &TextItem)> = items_data.iter().enumerate().collect();
        let cols = find_column_boundaries(&items, TableDetectionMode::SmallFont);
        // Only the cluster at x=100 should survive
        assert!(cols.len() <= 1);
    }

    // --- wrapped_line_rows ---

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|cell| cell.to_string()).collect()
    }

    #[test]
    fn wrapped_line_folds_into_its_row() {
        // Upstream #315: 16.5 pt rows, an 11.25 pt wrapped line.
        let rows = [720.0, 703.5, 692.25, 675.75];
        let cells = vec![
            row(&["Service", "Frequency", "Delivery", "Provider", "Location"]),
            row(&[
                "Instruction \u{2014}",
                "3x per",
                "pull-",
                "Special Education",
                "Lab",
            ]),
            row(&["Reading/Writing", "week", "out", "Teacher", ""]),
            row(&["Support Lab", "As needed", "direct", "Staff", "Center"]),
        ];
        let wrapped = wrapped_line_rows(&rows, &cells);
        assert_eq!(wrapped, [false, false, true, false]);
        let folded = fold_wrapped_rows(cells, &wrapped, |a, b| join_wrapped_cells(a, b));
        assert_eq!(folded.len(), 3);
        assert_eq!(
            folded[1],
            row(&[
                "Instruction \u{2014} Reading/Writing",
                "3x per week",
                "pull- out",
                "Special Education Teacher",
                "Lab"
            ])
        );
    }

    #[test]
    fn rows_at_one_pitch_or_without_continuing_text_stay() {
        // Uniform pitch: lowercase cells alone fold nothing.
        let rows = [700.0, 688.0, 676.0];
        let cells = vec![
            row(&["term", "meaning"]),
            row(&["apple", "a fruit"]),
            row(&["beet", ""]),
        ];
        assert_eq!(wrapped_line_rows(&rows, &cells), [false; 3]);
        // Grouped rows: a new label under a full row, at line pitch, is a row.
        let rows = [700.0, 688.0, 664.0, 652.0];
        let cells = vec![
            row(&["Revenue", "100", "120"]),
            row(&["Cost of sales", "60", "70"]),
            row(&["Operating expenses", "", ""]),
            row(&["Salaries", "20", "22"]),
        ];
        assert_eq!(wrapped_line_rows(&rows, &cells), [false; 4]);
        // A record under the header at line pitch is data once it holds a
        // number, however its words start.
        let rows = [720.0, 708.0, 684.0, 660.0];
        let cells = vec![
            row(&["Product", "Quantity", "Description", "Notes"]),
            row(&["widget", "12", "blue", ""]),
            row(&["gadget", "7", "red", "new"]),
            row(&["sprocket", "3", "green", "old"]),
        ];
        assert_eq!(wrapped_line_rows(&rows, &cells), [false; 4]);
        // Bare numbers under a row are data.
        let rows = [700.0, 688.0, 664.0];
        let cells = vec![
            row(&["Total", "100", "120"]),
            row(&["", "10", ""]),
            row(&["Net", "90", "100"]),
        ];
        assert_eq!(wrapped_line_rows(&rows, &cells), [false; 3]);
    }

    // --- find_row_boundaries ---

    #[test]
    fn test_find_row_boundaries_empty() {
        let items: Vec<(usize, &TextItem)> = vec![];
        assert_eq!(find_row_boundaries(&items), Vec::<f32>::new());
    }

    #[test]
    fn test_find_row_boundaries_descending_order() {
        let items_data = vec![
            make_item("A", 100.0, 500.0, 10.0),
            make_item("B", 100.0, 480.0, 10.0),
            make_item("C", 100.0, 460.0, 10.0),
        ];
        let items: Vec<(usize, &TextItem)> = items_data.iter().enumerate().collect();
        let rows = find_row_boundaries(&items);
        assert_eq!(rows.len(), 3);
        // Should be in descending order
        assert!(rows[0] > rows[1]);
        assert!(rows[1] > rows[2]);
    }

    #[test]
    fn test_find_row_boundaries_clustering() {
        // Items close together should cluster into one row
        let items_data = vec![
            make_item("A", 100.0, 500.0, 10.0),
            make_item("B", 200.0, 501.0, 10.0),
            make_item("C", 100.0, 480.0, 10.0),
        ];
        let items: Vec<(usize, &TextItem)> = items_data.iter().enumerate().collect();
        let rows = find_row_boundaries(&items);
        assert_eq!(rows.len(), 2); // 500 and 501 cluster together
    }

    #[test]
    fn test_find_row_boundaries_single_row() {
        let items_data = vec![make_item("A", 100.0, 500.0, 10.0)];
        let items: Vec<(usize, &TextItem)> = items_data.iter().enumerate().collect();
        let rows = find_row_boundaries(&items);
        assert_eq!(rows.len(), 1);
        assert!((rows[0] - 500.0).abs() < 0.01);
    }

    #[test]
    fn test_find_row_boundaries_items_at_same_y() {
        let items_data = vec![
            make_item("A", 100.0, 500.0, 10.0),
            make_item("B", 200.0, 500.0, 10.0),
            make_item("C", 300.0, 500.0, 10.0),
        ];
        let items: Vec<(usize, &TextItem)> = items_data.iter().enumerate().collect();
        let rows = find_row_boundaries(&items);
        assert_eq!(rows.len(), 1);
    }

    // --- recover_header_row ---

    #[test]
    fn test_recover_header_row_prepends_header() {
        let all_items = vec![
            make_item("Col1", 100.0, 520.0, 12.0), // body font, above table
            make_item("Col2", 200.0, 520.0, 12.0), // body font, above table
            make_item("A", 100.0, 500.0, 8.0),     // small font, in table
            make_item("B", 200.0, 500.0, 8.0),
        ];
        let mut table = Table {
            columns: vec![100.0, 200.0],
            rows: vec![500.0, 480.0],
            cells: vec![vec!["A".into(), "B".into()], vec!["C".into(), "D".into()]],
            item_indices: vec![2, 3],
            kind: TableKind::Data,
            layout_rows: Vec::new(),
        };

        recover_header_row(&mut table, &all_items, 9.0);
        assert_eq!(table.cells.len(), 3);
        assert_eq!(table.cells[0], vec!["Col1", "Col2"]);
    }

    #[test]
    fn test_recover_header_row_no_candidates() {
        let all_items = vec![
            make_item("A", 100.0, 500.0, 8.0),
            make_item("B", 200.0, 500.0, 8.0),
        ];
        let mut table = Table {
            columns: vec![100.0, 200.0],
            rows: vec![500.0],
            cells: vec![vec!["A".into(), "B".into()]],
            item_indices: vec![0, 1],
            kind: TableKind::Data,
            layout_rows: Vec::new(),
        };

        let rows_before = table.rows.len();
        recover_header_row(&mut table, &all_items, 9.0);
        assert_eq!(table.rows.len(), rows_before);
    }

    #[test]
    fn test_recover_header_row_skips_strikeout_candidates() {
        let mut old_col1 = make_item("Old Col1", 100.0, 520.0, 12.0);
        old_col1.is_strikeout = true;
        let mut old_col2 = make_item("Old Col2", 200.0, 520.0, 12.0);
        old_col2.is_strikeout = true;
        let all_items = vec![
            old_col1,
            old_col2,
            make_item("A", 100.0, 500.0, 8.0),
            make_item("B", 200.0, 500.0, 8.0),
        ];
        let mut table = Table {
            columns: vec![100.0, 200.0],
            rows: vec![500.0, 480.0],
            cells: vec![vec!["A".into(), "B".into()], vec!["C".into(), "D".into()]],
            item_indices: vec![2, 3],
            kind: TableKind::Data,
            layout_rows: Vec::new(),
        };

        recover_header_row(&mut table, &all_items, 9.0);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.cells[0], vec!["A", "B"]);
    }

    #[test]
    fn test_recover_header_row_too_far_above() {
        let all_items = vec![
            make_item("Col1", 100.0, 600.0, 12.0), // way above
            make_item("Col2", 200.0, 600.0, 12.0),
            make_item("A", 100.0, 500.0, 8.0),
            make_item("B", 200.0, 500.0, 8.0),
        ];
        let mut table = Table {
            columns: vec![100.0, 200.0],
            rows: vec![500.0, 480.0],
            cells: vec![vec!["A".into(), "B".into()], vec!["C".into(), "D".into()]],
            item_indices: vec![2, 3],
            kind: TableKind::Data,
            layout_rows: Vec::new(),
        };

        let rows_before = table.rows.len();
        recover_header_row(&mut table, &all_items, 9.0);
        assert_eq!(table.rows.len(), rows_before);
    }

    #[test]
    fn test_recover_header_row_single_column_populated() {
        // Only 1 column populated → not a real header
        let all_items = vec![
            make_item("OnlyCol1", 100.0, 520.0, 12.0),
            make_item("A", 100.0, 500.0, 8.0),
            make_item("B", 200.0, 500.0, 8.0),
        ];
        let mut table = Table {
            columns: vec![100.0, 200.0],
            rows: vec![500.0],
            cells: vec![vec!["A".into(), "B".into()]],
            item_indices: vec![1, 2],
            kind: TableKind::Data,
            layout_rows: Vec::new(),
        };

        let rows_before = table.rows.len();
        recover_header_row(&mut table, &all_items, 9.0);
        assert_eq!(table.rows.len(), rows_before);
    }

    #[test]
    fn test_recover_header_row_empty_table() {
        let all_items = vec![make_item("Col1", 100.0, 520.0, 12.0)];
        let mut table = Table {
            columns: vec![],
            rows: vec![],
            cells: vec![],
            item_indices: vec![],
            kind: TableKind::Data,
            layout_rows: Vec::new(),
        };

        recover_header_row(&mut table, &all_items, 9.0);
        assert!(table.cells.is_empty());
    }

    #[test]
    fn test_find_column_boundaries_dense_schedule() {
        // Simulate a 24-column train schedule with ~26pt column spacing and
        // per-glyph items that create many X-positions within each column.
        let mut items: Vec<(usize, TextItem)> = Vec::new();
        let mut rng_offset = 0.0f32;
        for col in 0..24 {
            let base_x = 50.0 + col as f32 * 26.0;
            // ~50 items per column with ±2pt jitter to simulate per-glyph text
            for row in 0..50 {
                rng_offset = (rng_offset + 0.7) % 4.0; // deterministic pseudo-jitter
                let x = base_x + rng_offset - 2.0;
                let y = 700.0 - row as f32 * 12.0;
                items.push((
                    0,
                    TextItem {
                        text: format!("{}", row),
                        x,
                        y,
                        width: 8.0,
                        font_size: 7.0,
                        height: 7.0,
                        font: String::new(),
                        font_tag: String::new(),
                        legacy_symbol_rewrite: false,
                        is_bold: false,
                        is_italic: false,
                        font_weight: None,
                        bold_source: None,
                        fixed_pitch: None,
                        fill_color: None,
                        stroke_color: None,
                        render_mode: None,
                        is_underline: false,
                        is_strikeout: false,
                        layout_strikeout: false,
                        rotation: 0.0,
                        advance_known: true,
                        item_type: ItemType::Text,
                        mcid: None,
                        baseline_shift: 0.0,
                        page: 1,
                    },
                ));
            }
        }
        let refs: Vec<(usize, &TextItem)> = items.iter().map(|(i, t)| (*i, t)).collect();
        let cols = find_column_boundaries(&refs, TableDetectionMode::SmallFont);
        // Should find close to 24 columns (within ±2)
        assert!(
            cols.len() >= 22 && cols.len() <= 26,
            "Expected ~24 columns, got {}",
            cols.len()
        );
    }

    #[test]
    fn test_find_column_boundaries_wide_spacing_still_works() {
        // Normal table with 4 widely-spaced columns — should still work
        let mut items = Vec::new();
        for col in 0..4 {
            let base_x = 50.0 + col as f32 * 120.0;
            for row in 0..10 {
                items.push((
                    0,
                    TextItem {
                        text: format!("cell_{}_{}", col, row),
                        x: base_x + (row as f32 * 0.3),
                        y: 700.0 - row as f32 * 15.0,
                        width: 40.0,
                        font_size: 10.0,
                        height: 7.0,
                        font: String::new(),
                        font_tag: String::new(),
                        legacy_symbol_rewrite: false,
                        is_bold: false,
                        is_italic: false,
                        font_weight: None,
                        bold_source: None,
                        fixed_pitch: None,
                        fill_color: None,
                        stroke_color: None,
                        render_mode: None,
                        is_underline: false,
                        is_strikeout: false,
                        layout_strikeout: false,
                        rotation: 0.0,
                        advance_known: true,
                        item_type: ItemType::Text,
                        mcid: None,
                        baseline_shift: 0.0,
                        page: 1,
                    },
                ));
            }
        }
        let refs: Vec<(usize, &TextItem)> = items.iter().map(|(i, t)| (*i, t)).collect();
        let cols = find_column_boundaries(&refs, TableDetectionMode::BodyFont);
        assert_eq!(cols.len(), 4, "Expected 4 columns, got {}", cols.len());
    }
}
