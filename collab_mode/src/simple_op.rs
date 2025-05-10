use crate::op::Operation;

/// Simple char-based op used for testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimpleOp {
    Ins(u64, char),
    Del(u64, Option<char>),
}

impl Operation for SimpleOp {
    fn inverse(&mut self) {
        todo!()
    }

    fn transform(&self, base: &SimpleOp, self_site: &SiteId, base_site: &SiteId) -> SimpleOp {
        match (self, base) {
            (SimpleOp::Ins(pos1, char1), SimpleOp::Ins(pos2, _)) => {
                if pos_less_than(*pos1, *pos2, self_site, base_site) {
                    SimpleOp::Ins(*pos1, *char1)
                } else {
                    SimpleOp::Ins(pos1 + 1, *char1)
                }
            }
            (SimpleOp::Ins(pos1, char1), SimpleOp::Del(pos2, Some(_))) => {
                if pos_less_than(*pos1, *pos2, self_site, base_site) {
                    SimpleOp::Ins(*pos1, *char1)
                } else {
                    SimpleOp::Ins(pos1 - 1, *char1)
                }
            }
            (SimpleOp::Ins(pos1, char1), SimpleOp::Del(_, None)) => SimpleOp::Ins(*pos1, *char1),
            (SimpleOp::Del(pos1, char1), SimpleOp::Ins(pos2, _)) => {
                if pos_less_than(*pos1, *pos2, self_site, base_site) {
                    SimpleOp::Del(*pos1, *char1)
                } else {
                    SimpleOp::Del(pos1 + 1, *char1)
                }
            }
            (SimpleOp::Del(pos1, char1), SimpleOp::Del(pos2, Some(_))) => {
                if *pos1 < *pos2 {
                    SimpleOp::Del(*pos1, *char1)
                } else if *pos1 == *pos2 {
                    SimpleOp::Del(*pos1, None)
                } else {
                    SimpleOp::Del(pos1 - 1, *char1)
                }
            }
            (SimpleOp::Del(pos1, char1), SimpleOp::Del(_, None)) => SimpleOp::Del(*pos1, *char1),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    
    #[test]
    fn test_simple_transform() {
        println!("Ins Ins, op < base.");
        test(
            SimpleOp::Ins(1, 'x'),
            SimpleOp::Ins(2, 'y'),
            SimpleOp::Ins(1, 'x'),
        );

        println!("Ins Ins, op > base.");
        test(
            SimpleOp::Ins(2, 'x'),
            SimpleOp::Ins(1, 'y'),
            SimpleOp::Ins(3, 'x'),
        );

        println!("Ins Del, op < base.");
        test(
            SimpleOp::Ins(1, 'x'),
            SimpleOp::Del(2, Some('y')),
            SimpleOp::Ins(1, 'x'),
        );

        println!("Ins Del, op > base.");
        test(
            SimpleOp::Ins(2, 'x'),
            SimpleOp::Del(1, Some('y')),
            SimpleOp::Ins(1, 'x'),
        );

        println!("Del Ins, op < base.");
        test(
            SimpleOp::Del(1, Some('x')),
            SimpleOp::Ins(2, 'y'),
            SimpleOp::Del(1, Some('x')),
        );

        println!("Del Ins, op > base.");
        test(
            SimpleOp::Del(2, Some('x')),
            SimpleOp::Ins(1, 'y'),
            SimpleOp::Del(3, Some('x')),
        );

        println!("Del Del, op < base.");
        test(
            SimpleOp::Del(1, Some('x')),
            SimpleOp::Del(2, Some('y')),
            SimpleOp::Del(1, Some('x')),
        );

        println!("Del Del, op = base.");
        test(
            SimpleOp::Del(1, Some('x')),
            SimpleOp::Del(1, Some('x')),
            SimpleOp::Del(1, None),
        );

        println!("Del Del, op > base.");
        test(
            SimpleOp::Del(2, Some('x')),
            SimpleOp::Del(1, Some('y')),
            SimpleOp::Del(1, Some('x')),
        );
    }
}
