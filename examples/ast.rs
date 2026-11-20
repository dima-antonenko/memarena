//! An expression tree lives in the arena and is freed by one reset of its owner.

use memarena::Arena;

enum Expr<'a> {
    Num(i64),
    Add(&'a Expr<'a>, &'a Expr<'a>),
}

fn num<'a>(arena: &'a Arena, value: i64) -> &'a Expr<'a> {
    arena.alloc(Expr::Num(value)).expect("арена")
}

fn add<'a>(arena: &'a Arena, lhs: &'a Expr<'a>, rhs: &'a Expr<'a>) -> &'a Expr<'a> {
    arena.alloc(Expr::Add(lhs, rhs)).expect("арена")
}

fn eval(expr: &Expr<'_>) -> i64 {
    match expr {
        Expr::Num(value) => *value,
        Expr::Add(lhs, rhs) => eval(lhs) + eval(rhs),
    }
}

fn main() {
    let arena = Arena::new();
    let expr = add(
        &arena,
        add(&arena, num(&arena, 1), num(&arena, 2)),
        num(&arena, 3),
    );
    let value = eval(expr);
    println!("(1 + 2) + 3 = {value}");
    assert_eq!(value, 6);
    println!("{}", arena.stats());
}
