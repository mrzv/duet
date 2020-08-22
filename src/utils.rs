use std::iter::{Peekable};
use std::cmp::Ordering;

pub struct MatchSorted_<T,U,K>
    where
        T: Iterator<Item = K>,
        U: Iterator<Item = K>,
        K: std::cmp::Ord,
{
    it1: T,
    it2: U,
}

pub type MatchSorted<T,U,K> = MatchSorted_<Peekable<T>,Peekable<U>,K>;

impl<T,U,K> MatchSorted<T,U,K>
    where
        T: Iterator<Item = K>,
        U: Iterator<Item = K>,
        K: std::cmp::Ord,
{
    pub fn new(it1: T, it2: U) -> MatchSorted<T,U,K> {
        MatchSorted { it1: it1.peekable(), it2: it2.peekable() }
    }
}

impl<T,U,K> Iterator for MatchSorted<T,U,K>
    where
        T: Iterator<Item = K>,
        U: Iterator<Item = K>,
        K: std::cmp::Ord,
{
    type Item = (Option<T::Item>, Option<U::Item>);

    fn next(&mut self) -> Option<Self::Item> {
        let x = self.it1.peek();
        let y = self.it2.peek();

        let (x,y) = match (&x,&y) {
            (Some(x), Some(y)) => (x,y),
            (None, None) => return None,
            _ => return Some((self.it1.next(),self.it2.next())),
        };

        let xk = x;
        let yk = y;

        match xk.cmp(yk) {
            Ordering::Less    => Some((self.it1.next(), None)),
            Ordering::Greater => Some((None, self.it2.next())),
            Ordering::Equal   => Some((self.it1.next(), self.it2.next())),
        }
    }
}

pub fn match_sorted<T,U,K>(it1: T, it2: U) -> MatchSorted<T,U,K>
    where
        T: Iterator<Item = K>,
        U: Iterator<Item = K>,
        K: std::cmp::Ord,
{
    MatchSorted::new(it1,it2)
}

#[test]
fn test_match_sorted() {
    let x = vec!(1,2,3,6,9);
    let y = vec!(2,5,6,7);

    //let z = MatchSorted::new(x.iter(),y.iter(), |a| a, |a| a);
    for (x,y) in match_sorted(x.iter(), y.iter()) {
        println!("{:?} {:?}", x,y);
    }
}
