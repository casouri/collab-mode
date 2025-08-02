//! Comprehensive tests for the InternalDoc struct

use super::*;

// Helper function to create a cursor
fn create_cursor(editor_pos: u64, internal_pos: u64, range_idx: Option<usize>) -> Cursor {
    Cursor {
        editor_pos,
        internal_pos,
        range_idx,
    }
}

// Helper function to compare ranges
fn assert_ranges_eq(actual: &[Range], expected: &[Range]) {
    assert_eq!(actual.len(), expected.len(), "Range count mismatch");
    for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
        assert_eq!(a, e, "Range mismatch at index {}", i);
    }
}

#[test]
fn test_internal_doc_new() {
    // Test empty document
    let doc = InternalDoc::new(0);
    assert_eq!(doc.ranges.len(), 0);
    assert_eq!(doc.cursors.len(), 0);

    // Test document with initial content
    let doc = InternalDoc::new(100);
    assert_eq!(doc.ranges.len(), 1);
    assert_eq!(doc.ranges[0], Range::Live(100));
    assert_eq!(doc.cursors.len(), 0);
}

#[test]
fn test_range_methods() {
    // Test Live range
    let live_range = Range::Live(50);
    assert!(live_range.is_live());
    assert!(!live_range.is_dead());
    assert_eq!(live_range.len(), 50);
    assert_eq!(live_range.live_len(), 50);

    // Test Dead range
    let dead_range = Range::Dead(30);
    assert!(!dead_range.is_live());
    assert!(dead_range.is_dead());
    assert_eq!(dead_range.len(), 30);
    assert_eq!(dead_range.live_len(), 0);
}

#[test]
fn test_editor_len() {
    let mut doc = InternalDoc::new(0);

    // Empty document
    assert_eq!(doc.editor_len(), 0);

    // Only live ranges
    doc.ranges = vec![Range::Live(10), Range::Live(20)];
    assert_eq!(doc.editor_len(), 30);

    // Mixed live and dead ranges
    doc.ranges = vec![
        Range::Live(10),
        Range::Dead(15),
        Range::Live(20),
        Range::Dead(5),
        Range::Live(5),
    ];
    assert_eq!(doc.editor_len(), 35);

    // Only dead ranges
    doc.ranges = vec![Range::Dead(10), Range::Dead(20)];
    assert_eq!(doc.editor_len(), 0);
}

#[test]
fn test_get_cursor() {
    let mut doc = InternalDoc::new(0);
    let site_id = 1;

    // First access creates a new cursor
    let cursor = doc.get_cursor(&site_id);
    assert_eq!(cursor.editor_pos, 0);
    assert_eq!(cursor.internal_pos, 0);
    assert_eq!(cursor.range_idx, None);

    // Modify cursor in map
    doc.cursors.insert(site_id, create_cursor(10, 15, Some(2)));

    // Get cursor returns the stored cursor
    let cursor = doc.get_cursor(&site_id);
    assert_eq!(cursor.editor_pos, 10);
    assert_eq!(cursor.internal_pos, 15);
    assert_eq!(cursor.range_idx, Some(2));
}

#[test]
fn test_convert_pos_empty_doc() {
    let doc = InternalDoc::new(0);
    let mut cursor = create_cursor(0, 0, None);

    // Converting position 0 in empty doc
    assert_eq!(doc.convert_pos(0, &mut cursor, true), 0);
    assert_eq!(doc.convert_pos(0, &mut cursor, false), 0);

    // Any non-zero position in empty doc returns 0
    assert_eq!(doc.convert_pos(10, &mut cursor, true), 0);
    assert_eq!(doc.convert_pos(10, &mut cursor, false), 0);
}

#[test]
fn test_convert_pos_simple() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(20)];
    let mut cursor = doc.get_cursor(&0);

    // Editor to internal (all positions are the same in a live range)
    assert_eq!(doc.convert_pos(0, &mut cursor, true), 0);
    assert_eq!(doc.convert_pos(10, &mut cursor, true), 10);
    assert_eq!(doc.convert_pos(20, &mut cursor, true), 20);

    // Internal to editor (all positions are the same in a live range)
    cursor = doc.get_cursor(&0);
    assert_eq!(doc.convert_pos(0, &mut cursor, false), 0);
    assert_eq!(doc.convert_pos(10, &mut cursor, false), 10);
    assert_eq!(doc.convert_pos(20, &mut cursor, false), 20);
}

#[test]
fn test_convert_pos_with_tombstones() {
    let mut doc = InternalDoc::new(0);
    // [Live(10)][Dead(5)][Live(10)]
    // Editor: 0-10, 10-20
    // Internal: 0-10, 10-15, 15-25
    doc.ranges = vec![Range::Live(10), Range::Dead(5), Range::Live(10)];
    let mut cursor = doc.get_cursor(&0);

    // Editor to internal conversion
    assert_eq!(doc.convert_pos(0, &mut cursor, true), 0);
    assert_eq!(doc.convert_pos(5, &mut cursor, true), 5);
    assert_eq!(doc.convert_pos(10, &mut cursor, true), 10); // Beginning of dead range
    assert_eq!(doc.convert_pos(11, &mut cursor, true), 16); // After dead range
    assert_eq!(doc.convert_pos(20, &mut cursor, true), 25);

    // Internal to editor conversion
    cursor = doc.get_cursor(&0);
    assert_eq!(doc.convert_pos(0, &mut cursor, false), 0);
    assert_eq!(doc.convert_pos(5, &mut cursor, false), 5);
    assert_eq!(doc.convert_pos(10, &mut cursor, false), 10); // Beginning of dead range
    assert_eq!(doc.convert_pos(12, &mut cursor, false), 10); // Middle of dead range
    assert_eq!(doc.convert_pos(15, &mut cursor, false), 10); // End of dead range
    assert_eq!(doc.convert_pos(16, &mut cursor, false), 11); // Beginning of next live range
    assert_eq!(doc.convert_pos(25, &mut cursor, false), 20);
}

#[test]
fn test_shift_cursors_right() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(10), Range::Dead(5), Range::Live(10)];

    // Add cursors at different positions
    doc.cursors.insert(1, create_cursor(5, 5, Some(0)));
    doc.cursors.insert(2, create_cursor(10, 15, Some(2)));
    doc.cursors.insert(3, create_cursor(15, 20, Some(2)));

    // Shift cursors from index 2 onwards by 10 positions, 1 range index
    doc.shift_cursors_right(2, 10, 1);

    // Cursor 1 should not be affected (before index 2)
    let cursor1 = doc.cursors.get(&1).unwrap();
    assert_eq!(cursor1.editor_pos, 5);
    assert_eq!(cursor1.internal_pos, 5);
    assert_eq!(cursor1.range_idx, Some(0));

    // Cursors 2 and 3 should be shifted
    let cursor2 = doc.cursors.get(&2).unwrap();
    assert_eq!(cursor2.editor_pos, 20);
    assert_eq!(cursor2.internal_pos, 25);
    assert_eq!(cursor2.range_idx, Some(3));

    let cursor3 = doc.cursors.get(&3).unwrap();
    assert_eq!(cursor3.editor_pos, 25);
    assert_eq!(cursor3.internal_pos, 30);
    assert_eq!(cursor3.range_idx, Some(3));
}

#[test]
fn test_apply_ins_empty_doc() {
    let mut doc = InternalDoc::new(0);
    let mut cursor = doc.get_cursor(&0);

    // Insert into empty document
    doc.apply_ins(0, 10, &mut cursor);
    assert_ranges_eq(&doc.ranges, &[Range::Live(10)]);
    assert_eq!(cursor.internal_pos, 0);
    assert_eq!(cursor.editor_pos, 0);
}

#[test]
fn test_apply_ins_into_live_range() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(20)];
    let mut cursor = doc.get_cursor(&0);
    doc.convert_pos(10, &mut cursor, true);

    // Insert into middle of live range
    doc.apply_ins(10, 5, &mut cursor);
    assert_ranges_eq(&doc.ranges, &[Range::Live(25)]);
    assert_eq!(cursor.internal_pos, 0);
    assert_eq!(cursor.range_idx, Some(0));
}

#[test]
fn test_apply_ins_at_dead_range_boundaries() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(10), Range::Dead(10), Range::Live(10)];
    let mut cursor = doc.get_cursor(&0);

    // Insert at beginning of dead range (should insert before)
    doc.convert_pos(10, &mut cursor, true);
    doc.apply_ins(10, 5, &mut cursor);
    assert_ranges_eq(
        &doc.ranges,
        &[Range::Live(15), Range::Dead(10), Range::Live(10)],
    );

    // Reset
    doc.ranges = vec![Range::Live(10), Range::Dead(10), Range::Live(10)];
    cursor = doc.get_cursor(&0);

    // Insert at end of dead range (should insert after)
    doc.convert_pos(20, &mut cursor, false);
    doc.apply_ins(20, 5, &mut cursor);
    assert_ranges_eq(
        &doc.ranges,
        &[Range::Live(10), Range::Dead(10), Range::Live(15)],
    );
}

#[test]
fn test_apply_ins_split_dead_range() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Dead(20)];
    let mut cursor = doc.get_cursor(&0);
    doc.convert_pos(10, &mut cursor, false);

    // Insert in middle of dead range
    doc.apply_ins(10, 5, &mut cursor);
    assert_ranges_eq(
        &doc.ranges,
        &[Range::Dead(10), Range::Live(5), Range::Dead(10)],
    );
    assert_eq!(cursor.internal_pos, 10);
    assert_eq!(cursor.range_idx, Some(1));
}

#[test]
fn test_apply_mark_dead_simple() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(30)];
    let mut cursor = doc.get_cursor(&0);
    doc.convert_pos(10, &mut cursor, true);

    // Mark middle portion as dead
    let flipped = doc.apply_mark_dead(10, 10, &mut cursor);
    assert_ranges_eq(
        &doc.ranges,
        &[Range::Live(10), Range::Dead(10), Range::Live(10)],
    );
    assert_eq!(flipped, vec![(10, 10)]);
}

#[test]
fn test_apply_mark_dead_already_dead() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Dead(20)];
    let mut cursor = doc.get_cursor(&0);
    doc.convert_pos(0, &mut cursor, false);

    // Mark already dead range
    let flipped = doc.apply_mark_dead(0, 20, &mut cursor);
    assert_ranges_eq(&doc.ranges, &[Range::Dead(20)]);
    assert_eq!(flipped, vec![]); // Nothing was flipped
}

#[test]
fn test_apply_mark_dead_complex() {
    let mut doc = InternalDoc::new(0);
    // [Live(10)][Dead(5)][Live(5)][Dead(10)][Live(10)]
    doc.ranges = vec![
        Range::Live(10),
        Range::Dead(5),
        Range::Live(5),
        Range::Dead(10),
        Range::Live(10),
    ];
    let mut cursor = doc.get_cursor(&0);
    doc.convert_pos(0, &mut cursor, false);

    // Mark across multiple ranges (6 to 36)
    let flipped = doc.apply_mark_dead(6, 30, &mut cursor);
    assert_eq!(flipped, vec![(6, 4), (15, 5), (30, 6)]);
    // Should coalesce adjacent dead ranges
    assert_ranges_eq(
        &doc.ranges,
        &[Range::Live(6), Range::Dead(30), Range::Live(4)],
    );
}

#[test]
fn test_apply_mark_live_simple() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Dead(30)];
    let mut cursor = doc.get_cursor(&0);
    doc.convert_pos(10, &mut cursor, false);

    // Mark middle portion as live
    doc.apply_mark_live(10, 10, &mut cursor);
    assert_ranges_eq(
        &doc.ranges,
        &[Range::Dead(10), Range::Live(10), Range::Dead(10)],
    );
}

#[test]
fn test_apply_mark_live_already_live() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(20)];
    let mut cursor = doc.get_cursor(&0);
    doc.convert_pos(0, &mut cursor, false);

    // Mark already live range
    doc.apply_mark_live(0, 20, &mut cursor);
    assert_ranges_eq(&doc.ranges, &[Range::Live(20)]);
}

#[test]
fn test_convert_editor_op_ins() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(10), Range::Dead(5), Range::Live(10)];
    let site_id = 1;

    // Insert at editor position 15 (after dead range)
    let editor_op = EditorOp::Ins(15, "hello".to_string());
    let internal_op = doc.convert_editor_op_and_apply(editor_op, site_id);

    match internal_op {
        Op::Ins((pos, text), site) => {
            assert_eq!(pos, 20); // Internal position after dead range
            assert_eq!(text, "hello");
            assert_eq!(site, site_id);
        }
        _ => panic!("Expected Ins op"),
    }

    // Check document was updated
    assert_ranges_eq(
        &doc.ranges,
        &[Range::Live(10), Range::Dead(5), Range::Live(15)],
    );
}

#[test]
fn test_convert_editor_op_del() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(20)];
    let site_id = 1;

    // Delete from editor position 5 to 15
    let editor_op = EditorOp::Del(5, "0123456789".to_string());
    let internal_op = doc.convert_editor_op_and_apply(editor_op, site_id);

    match internal_op {
        Op::Mark(edits, live, site) => {
            assert!(!live); // Should be marking as dead
            assert_eq!(edits.len(), 1);
            assert_eq!(edits[0].0, 5); // Position
            assert_eq!(edits[0].1, "0123456789"); // Text
            assert_eq!(site, site_id);
        }
        _ => panic!("Expected Mark op"),
    }

    // Check document was updated
    assert_ranges_eq(
        &doc.ranges,
        &[Range::Live(5), Range::Dead(10), Range::Live(5)],
    );
}

#[test]
fn test_convert_internal_op_ins() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(10), Range::Dead(5), Range::Live(10)];
    let site_id = 1;

    // Insert at internal position 20 (maps to editor position 15)
    let internal_op = Op::Ins((20, "world".to_string()), site_id);
    let edit_instr = doc.convert_internal_op_and_apply(internal_op);

    match edit_instr {
        EditInstruction::Ins(edits) => {
            assert_eq!(edits.len(), 1);
            assert_eq!(edits[0].0, 15); // Editor position
            assert_eq!(edits[0].1, "world");
        }
        _ => panic!("Expected Ins instruction"),
    }
}

#[test]
fn test_convert_internal_op_mark_dead() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(30)];
    let site_id = 1;

    // Mark positions 10-20 as dead
    let internal_op = Op::Mark(vec![(10, "deleted123".to_string())], false, site_id);
    let edit_instr = doc.convert_internal_op_and_apply(internal_op);

    match edit_instr {
        EditInstruction::Del(edits) => {
            assert_eq!(edits.len(), 1);
            assert_eq!(edits[0].0, 10); // Editor position
            assert_eq!(edits[0].1, "deleted123");
        }
        _ => panic!("Expected Del instruction"),
    }
}

#[test]
fn test_convert_internal_op_mark_live() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Dead(30)];
    let site_id = 1;

    // Mark positions 10-20 as live (resurrection)
    let internal_op = Op::Mark(vec![(10, "restored12".to_string())], true, site_id);
    let edit_instr = doc.convert_internal_op_and_apply(internal_op);

    match edit_instr {
        EditInstruction::Ins(edits) => {
            assert_eq!(edits.len(), 1);
            assert_eq!(edits[0].0, 0); // Editor position (at beginning since all was dead)
            assert_eq!(edits[0].1, "restored12");
        }
        _ => panic!("Expected Ins instruction"),
    }
}

#[test]
fn test_cursor_movement() {
    let cursor1 = create_cursor(10, 20, Some(2));
    let mut cursor2 = create_cursor(0, 0, None);

    // Test move_to_cursor
    cursor2.move_to_cursor(&cursor1);
    assert_eq!(cursor2.editor_pos, 10);
    assert_eq!(cursor2.internal_pos, 20);
    assert_eq!(cursor2.range_idx, Some(2));

    // Test shift
    cursor2.shift(5, 1);
    assert_eq!(cursor2.editor_pos, 15);
    assert_eq!(cursor2.range_idx, Some(3));

    cursor2.shift(-3, -1);
    assert_eq!(cursor2.editor_pos, 12);
    assert_eq!(cursor2.range_idx, Some(2));
}

#[test]
fn test_range_iterator() {
    let ranges = vec![
        Range::Live(10),
        Range::Dead(5),
        Range::Live(15),
        Range::Dead(10),
    ];
    let cursor = create_cursor(10, 15, Some(2));
    let mut iter = RangeIterator::new(&ranges, &cursor);

    // Initial state
    assert_eq!(iter.range_idx, 2);
    assert_eq!(iter.range_beg, 15);
    assert_eq!(iter.range_end, 30);
    assert!(iter.is_live);
    assert_eq!(iter.editor_range_beg, 10);
    assert_eq!(iter.editor_range_end, 25);

    // Move right
    iter.move_right(&ranges);
    assert_eq!(iter.range_idx, 3);
    assert_eq!(iter.range_beg, 30);
    assert_eq!(iter.range_end, 40);
    assert!(!iter.is_live);
    assert_eq!(iter.editor_range_beg, 25);
    assert_eq!(iter.editor_range_end, 25); // Dead range has 0 editor length

    // Move left back
    iter.move_left(&ranges);
    assert_eq!(iter.range_idx, 2);
    assert_eq!(iter.range_beg, 15);
    assert_eq!(iter.range_end, 30);
    assert!(iter.is_live);
}

#[test]
fn test_complex_scenario() {
    // Test a complex editing scenario
    let mut doc = InternalDoc::new(0);
    let site1 = 1;
    let site2 = 2;

    // Initial text: "Hello World" (11 chars)
    doc.ranges = vec![Range::Live(11)];

    // Site 1 deletes "World" (positions 6-11)
    let op1 = EditorOp::Del(6, "World".to_string());
    doc.convert_editor_op_and_apply(op1, site1);
    assert_ranges_eq(&doc.ranges, &[Range::Live(6), Range::Dead(5)]);

    // Site 2 inserts " there" at position 5
    let op2 = EditorOp::Ins(5, " there".to_string());
    doc.convert_editor_op_and_apply(op2, site2);
    assert_ranges_eq(&doc.ranges, &[Range::Live(12), Range::Dead(5)]);

    // Check final editor length
    assert_eq!(doc.editor_len(), 12); // "Hello there"
}

#[test]
fn test_unicode_handling() {
    let mut doc = InternalDoc::new(0);
    let site_id = 1;

    // Insert text with unicode characters
    let unicode_text = "Hello 👋 世界 🌍";
    let char_count = unicode_text.chars().count() as u64;

    let op = EditorOp::Ins(0, unicode_text.to_string());
    doc.convert_editor_op_and_apply(op, site_id);

    assert_eq!(doc.editor_len(), char_count);
    assert_ranges_eq(&doc.ranges, &[Range::Live(char_count)]);
}

#[test]
fn test_multiple_cursors_concurrent_edits() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(50)];

    // Three sites with cursors at different positions
    doc.cursors.insert(1, create_cursor(10, 10, Some(0)));
    doc.cursors.insert(2, create_cursor(20, 20, Some(0)));
    doc.cursors.insert(3, create_cursor(30, 30, Some(0)));

    // Site 1 inserts at position 10
    let mut cursor1 = doc.get_cursor(&1);
    doc.apply_ins(10, 5, &mut cursor1);

    // Check that cursors after the insertion point were shifted:
    // No, the purpose of the cursor is to map editor position with
    // internal position, so cursor 2 and 3 don't need to shift.
    let cursor2 = doc.cursors.get(&2).unwrap();
    assert_eq!(cursor2.editor_pos, 20); // Not shifted by 5
    assert_eq!(cursor2.internal_pos, 20);

    let cursor3 = doc.cursors.get(&3).unwrap();
    assert_eq!(cursor3.editor_pos, 30); // Not shifted by 5
    assert_eq!(cursor3.internal_pos, 30);
}

// Added check outside of internal doc to handle this.
// #[test]
// fn test_edge_cases() {
//     let mut doc = InternalDoc::new(0);
//     let site_id = 1;

//     Test empty insertion
//     let op = EditorOp::Ins(0, "".to_string());
//     doc.convert_editor_op_and_apply(op, site_id);
//     assert_eq!(doc.ranges.len(), 0);

//     Test deletion from empty document
//     doc = InternalDoc::new(0);
//     let op = EditorOp::Del(0, "".to_string());
//     let result = doc.convert_editor_op_and_apply(op, site_id);
//     match result {
//         Op::Mark(edits, _, _) => assert_eq!(edits.len(), 0),
//         _ => panic!("Expected Mark op"),
//     }
// }

#[test]
fn test_boundary_conditions() {
    let mut doc = InternalDoc::new(0);
    doc.ranges = vec![Range::Live(10), Range::Dead(10), Range::Live(10)];
    let mut cursor = doc.get_cursor(&0);

    // Test conversion at exact boundaries
    assert_eq!(doc.convert_pos(0, &mut cursor, true), 0); // Start of doc
    assert_eq!(doc.convert_pos(10, &mut cursor, true), 10); // Boundary between live and dead
    assert_eq!(doc.convert_pos(10, &mut cursor, false), 10); // Same for internal to editor
    assert_eq!(doc.convert_pos(20, &mut cursor, false), 10); // End of dead range
    assert_eq!(doc.convert_pos(30, &mut cursor, false), 20); // End of doc
}

#[test]
fn test_coalescing_ranges() {
    let mut doc = InternalDoc::new(0);
    let mut cursor = doc.get_cursor(&0);
    doc.apply_ins(0, 10, &mut cursor);

    // After any operation, adjacent ranges of same type should be merged
    doc.convert_pos(5, &mut cursor, true);
    doc.apply_ins(5, 5, &mut cursor);
    // Should result in a single live range, not multiple
    assert_eq!(doc.ranges.len(), 1);
    assert_eq!(doc.ranges[0], Range::Live(15));
}

#[test]
fn test_range_iterator_edge_cases() {
    let ranges = vec![Range::Live(10)];
    let cursor = create_cursor(0, 0, Some(0));
    let iter = RangeIterator::new(&ranges, &cursor);

    // Test initial state at beginning of doc
    assert_eq!(iter.range_idx, 0);
    assert_eq!(iter.range_beg, 0);
    assert_eq!(iter.range_end, 10);
    assert!(iter.is_live);
    assert_eq!(iter.editor_range_beg, 0);
    assert_eq!(iter.editor_range_end, 10);
}

#[test]
fn test_cursor_pos_helper() {
    let cursor = create_cursor(10, 20, Some(1));

    // Test the pos helper method
    assert_eq!(cursor.pos(true), 10); // Editor position
    assert_eq!(cursor.pos(false), 20); // Internal position
}

#[test]
fn test_move_to_beg() {
    let ranges = vec![Range::Live(10), Range::Dead(5), Range::Live(15)];
    let cursor = create_cursor(25, 30, Some(2));
    let iter = RangeIterator::new(&ranges, &cursor);

    let mut test_cursor = create_cursor(0, 0, None);
    test_cursor.move_to_beg(&iter);

    assert_eq!(test_cursor.internal_pos, iter.range_beg);
    assert_eq!(test_cursor.editor_pos, iter.editor_range_beg);
    assert_eq!(test_cursor.range_idx, Some(iter.range_idx));
}
