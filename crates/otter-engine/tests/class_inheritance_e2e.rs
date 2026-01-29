use otter_engine::EngineBuilder;

fn create_test_engine() -> otter_engine::Otter {
    EngineBuilder::new().build()
}

#[test]
fn test_basic_class_extends() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        class Animal {
            constructor(name) {
                this.name = name;
            }
            speak() {
                return this.name + " makes a noise";
            }
        }
        class Dog extends Animal {
            constructor(name) {
                super(name);
            }
            bark() {
                return this.name + " barks";
            }
        }
        let d = new Dog("Rex");
        d.bark()
    "#,
    );
    match result {
        Ok(v) => {
            assert!(v.is_string());
            assert_eq!(v.as_string().unwrap().as_str(), "Rex barks");
        }
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_instanceof_with_inheritance() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        class Animal {}
        class Dog extends Animal {}
        let d = new Dog();
        (d instanceof Dog) && (d instanceof Animal)
    "#,
    );
    match result {
        Ok(v) => assert_eq!(v.as_boolean(), Some(true)),
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_super_constructor_with_args() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        class Base {
            constructor(x, y) {
                this.x = x;
                this.y = y;
            }
        }
        class Derived extends Base {
            constructor(x, y, z) {
                super(x, y);
                this.z = z;
            }
        }
        let d = new Derived(1, 2, 3);
        d.x + d.y + d.z
    "#,
    );
    match result {
        Ok(v) => assert_eq!(v.as_number(), Some(6.0)),
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_inherited_method() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        class Animal {
            constructor(name) {
                this.name = name;
            }
            speak() {
                return this.name + " speaks";
            }
        }
        class Dog extends Animal {
            constructor(name) {
                super(name);
            }
        }
        let d = new Dog("Buddy");
        d.speak()
    "#,
    );
    match result {
        Ok(v) => {
            assert!(v.is_string());
            assert_eq!(v.as_string().unwrap().as_str(), "Buddy speaks");
        }
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_method_override() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        class Animal {
            speak() {
                return "animal";
            }
        }
        class Dog extends Animal {
            constructor() { super(); }
            speak() {
                return "dog";
            }
        }
        let d = new Dog();
        d.speak()
    "#,
    );
    match result {
        Ok(v) => {
            assert!(v.is_string());
            assert_eq!(v.as_string().unwrap().as_str(), "dog");
        }
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_multi_level_inheritance() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        class A {
            constructor() { this.a = 1; }
        }
        class B extends A {
            constructor() { super(); this.b = 2; }
        }
        class C extends B {
            constructor() { super(); this.c = 3; }
        }
        let c = new C();
        c.a + c.b + c.c
    "#,
    );
    match result {
        Ok(v) => assert_eq!(v.as_number(), Some(6.0)),
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_super_method_call() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        class Animal {
            speak() {
                return "animal";
            }
        }
        class Dog extends Animal {
            constructor() { super(); }
            speak() {
                return super.speak() + " dog";
            }
        }
        let d = new Dog();
        d.speak()
    "#,
    );
    match result {
        Ok(v) => {
            assert!(v.is_string());
            assert_eq!(v.as_string().unwrap().as_str(), "animal dog");
        }
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_super_property_access() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        class Base {
            get value() { return 42; }
        }
        class Derived extends Base {
            constructor() { super(); }
            getValue() {
                return super.value;
            }
        }
        let d = new Derived();
        d.getValue()
    "#,
    );
    match result {
        Ok(v) => assert_eq!(v.as_number(), Some(42.0)),
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_extends_null() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        class NullBase extends null {
            constructor() {
                // Cannot call super() when extending null
                return Object.create(NullBase.prototype);
            }
        }
        let n = new NullBase();
        n instanceof NullBase
    "#,
    );
    match result {
        Ok(v) => assert_eq!(v.as_boolean(), Some(true)),
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_static_inheritance() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        class Animal {
            static create() { return "animal"; }
        }
        class Dog extends Animal {}
        Dog.create()
    "#,
    );
    match result {
        Ok(v) => {
            assert!(v.is_string());
            assert_eq!(v.as_string().unwrap().as_str(), "animal");
        }
        Err(e) => panic!("Failed: {:?}", e),
    }
}
