use std::iter::{Peekable};
use std::cmp::Ordering;

pub struct MatchSorted<T,U,KT,KU,K>
    where
        T: Iterator,
        U: Iterator,
        KT: Fn(&T::Item) -> &K,
        KU: Fn(&U::Item) -> &K,
        K: std::cmp::Ord,
{
    it1: T,
    it2: U,
    key1: KT,
    key2: KU,
}

impl<T,U,KT,KU,K> MatchSorted<Peekable<T>,Peekable<U>,KT,KU,K>
    where
        T: Iterator,
        U: Iterator,
        KT: Fn(&T::Item) -> &K,
        KU: Fn(&U::Item) -> &K,
        K: std::cmp::Ord,
{
    pub fn new(it1: T, it2: U, key1: KT, key2: KU) -> MatchSorted<Peekable<T>,Peekable<U>,KT,KU,K> {
        MatchSorted { it1: it1.peekable(), it2: it2.peekable(), key1, key2 }
    }
}

impl<T,U,KT,KU,K> Iterator for MatchSorted<Peekable<T>,Peekable<U>,KT,KU,K>
    where
        T: Iterator,
        U: Iterator,
        KT: Fn(&T::Item) -> &K,
        KU: Fn(&U::Item) -> &K,
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

        let xk = (self.key1)(x);
        let yk = (self.key2)(y);

        match xk.cmp(yk) {
            Ordering::Less    => Some((self.it1.next(), None)),
            Ordering::Greater => Some((None, self.it2.next())),
            Ordering::Equal   => Some((self.it1.next(), self.it2.next())),
        }
    }
}

fn match_sorted<T,U,KT,KU,K>(r1: T, r2: U, key1: KT, key2: KU) -> MatchSorted<Peekable<T::IntoIter>,Peekable<U::IntoIter>,KT,KU,K>
    where
        T: IntoIterator,
        U: IntoIterator,
        KT: Fn(&T::Item) -> &K,
        KU: Fn(&U::Item) -> &K,
        K: std::cmp::Ord,
{
    MatchSorted::new(r1.into_iter(),r2.into_iter(),key1,key2)
}

#[test]
fn test_match_sorted() {
    let x = vec!(1,2,3,6,9);
    let y = vec!(2,5,6,7);

    //let z = MatchSorted::new(x.iter(),y.iter(), |a| a, |a| a);
    for (x,y) in match_sorted(x, y, |a| a, |a| a) {
        println!("{:?} {:?}", x,y);
    }
}
