//! `java.lang`: Object, System, Runtime, Class, Throwable and its
//! subclasses, StackTraceElement, Enum, Record and the core interfaces, and
//! the helpers the rest of the VM uses (boxing, equality, hashing, the
//! launcher's stack trace, JEP 358's messages).
use super::{
    A, ABSTRACT, FINAL, INTERFACE, M, NativeClass, PUBLIC, S, State, arg, class, ret_bool, ret_ref,
    ret_str, state, this, void, NATIVE};
use crate::natives;
use crate::vm::class::{ACC_ENUM, ClassKind};
use crate::vm::heap::{Obj, ThrowableData};
use crate::vm::interp::{Op, Vm};
use crate::vm::{Array, ClassId, Data, Exc, MethodId, ObjId, Res, Value};

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

static CLASSES: &[NativeClass] = &[
    NativeClass {
        name: "java/lang/Object",
        super_name: None,
        interfaces: &[],
        access: PUBLIC,
        methods: natives![
            // In HotSpot's order, and with Java's modifiers (reflection shows both).
            "<init>" "()V" M => Some(|_, _| void()),
            "equals" "(Ljava/lang/Object;)Z" M => Some(|_, a| ret_bool(arg(a, 0).r() == arg(a, 1).r())),
            "getClass" "()Ljava/lang/Class;" M | FINAL | NATIVE => Some(object_get_class),
            "notify" "()V" M | FINAL | NATIVE => Some(super::thread::object_notify),
            "notifyAll" "()V" M | FINAL | NATIVE => Some(super::thread::object_notify_all),
            "wait" "()V" M | FINAL => Some(super::thread::object_wait),
            "wait" "(J)V" M | FINAL => Some(super::thread::object_wait),
            "wait" "(JI)V" M | FINAL => Some(super::thread::object_wait),
            "toString" "()Ljava/lang/String;" M => Some(object_to_string),
            "hashCode" "()I" M | NATIVE => Some(object_hash_code),
            "clone" "()Ljava/lang/Object;" 0x0004 | NATIVE => Some(object_clone),
            "finalize" "()V" 0x0004 => Some(|_, _| void()),
        ],
        statics: &[],
        clinit: None,
        data: None,
    },
    class("java/lang/Cloneable", None, &[], INTERFACE, &[]),
    class("java/io/Serializable", None, &[], INTERFACE, &[]),
    class("java/lang/constant/Constable", None, &[], INTERFACE, &[]),
    class("java/lang/constant/ConstantDesc", None, &[], INTERFACE, &[]),
    class("java/lang/Runnable", None, &[], INTERFACE, natives!["run" "()V" A => None]),
    class("java/lang/AutoCloseable", None, &[], INTERFACE, natives!["close" "()V" A => None]),
    class("java/io/Closeable", None, &["java/lang/AutoCloseable"], INTERFACE, natives!["close" "()V" A => None]),
    class("java/lang/Comparable", None, &[], INTERFACE, natives!["compareTo" "(Ljava/lang/Object;)I" A => None]),
    class("java/lang/Appendable", None, &[], INTERFACE, natives![
        "append" "(Ljava/lang/CharSequence;)Ljava/lang/Appendable;" A => None,
        "append" "(C)Ljava/lang/Appendable;" A => None,
    ]),
    class("java/lang/Readable", None, &[], INTERFACE, &[]),
    class("java/lang/Iterable", None, &[], INTERFACE, natives![
        "iterator" "()Ljava/util/Iterator;" A => None,
        "forEach" "(Ljava/util/function/Consumer;)V" M => Some(iterable_for_each),
        "spliterator" "()Ljava/util/Spliterator;" M => Some(|_, _| Err(Exc::Unsupported("Iterable.spliterator".into()))),
    ]),
    class("java/lang/CharSequence", None, &[], INTERFACE, natives![
        "length" "()I" A => None,
        "charAt" "(I)C" A => None,
        "subSequence" "(II)Ljava/lang/CharSequence;" A => None,
        "toString" "()Ljava/lang/String;" A => None,
        "isEmpty" "()Z" M => Some(|vm, a| { let t = this(vm, a)?; let n = vm.invoke_virtual(t, "length", "()I", &[])?; ret_bool(n.i() == 0) }),
        "chars" "()Ljava/util/stream/IntStream;" M => Some(super::string::char_sequence_chars),
        "compare" "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)I" S => Some(super::string::char_sequence_compare),
    ]),
    NativeClass {
        name: "java/lang/System",
        super_name: Some("java/lang/Object"),
        interfaces: &[],
        access: PUBLIC | FINAL,
        methods: natives![
            "currentTimeMillis" "()J" S => Some(|vm, _| Ok(Value::Long(vm.options.epoch_millis + (vm.jdk.nanos / 1_000_000) as i64))),
            "nanoTime" "()J" S => Some(|vm, _| Ok(Value::Long(1_000_000_000 + vm.jdk.nanos as i64 + vm.instructions as i64))),
            "arraycopy" "(Ljava/lang/Object;ILjava/lang/Object;II)V" S => Some(system_arraycopy),
            "mapLibraryName" "(Ljava/lang/String;)Ljava/lang/String;" S => Some(|vm, a| { let n = vm.string_nn(arg(a, 0).r())?; ret_str(vm, &format!("lib{n}.so")) }),
            "identityHashCode" "(Ljava/lang/Object;)I" S => Some(|vm, a| Ok(Value::Int(vm.identity_hash(arg(a, 0).r())))),
            "exit" "(I)V" S => Some(|_, a| Err(Exc::Exit(arg(a, 0).i()))),
            "getProperty" "(Ljava/lang/String;)Ljava/lang/String;" S => Some(system_get_property),
            "getProperty" "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;" S => Some(system_get_property),
            "setProperty" "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;" S => Some(system_set_property),
            "clearProperty" "(Ljava/lang/String;)Ljava/lang/String;" S => Some(system_clear_property),
            "lineSeparator" "()Ljava/lang/String;" S => Some(|vm, _| ret_str(vm, "\n")),
            "getenv" "(Ljava/lang/String;)Ljava/lang/String;" S => Some(|_, _| ret_ref(0)),
            "getenv" "()Ljava/util/Map;" S => Some(|vm, _| { let m = super::util::new_map(vm, "java/util/HashMap")?; ret_ref(m) }),
            "setOut" "(Ljava/io/PrintStream;)V" S => Some(|vm, a| { let c = vm.load("java/lang/System")?; vm.set_static(c, "out", arg(a, 0)); void() }),
            "setErr" "(Ljava/io/PrintStream;)V" S => Some(|vm, a| { let c = vm.load("java/lang/System")?; vm.set_static(c, "err", arg(a, 0)); void() }),
            "setIn" "(Ljava/io/InputStream;)V" S => Some(|vm, a| { let c = vm.load("java/lang/System")?; vm.set_static(c, "in", arg(a, 0)); void() }),
            "gc" "()V" S => Some(|_, _| void()),
            "runFinalization" "()V" S => Some(|_, _| void()),
            "getSecurityManager" "()Ljava/lang/SecurityManager;" S => Some(|_, _| ret_ref(0)),
            "getLogger" "(Ljava/lang/String;)Ljava/lang/System$Logger;" S => Some(|_, _| Err(Exc::Unsupported("System.getLogger".into()))),
        ],
        statics: &[("out", "Ljava/io/PrintStream;"), ("err", "Ljava/io/PrintStream;"), ("in", "Ljava/io/InputStream;")],
        clinit: Some(system_clinit),
        data: None,
    },
    NativeClass {
        name: "java/lang/Runtime",
        super_name: Some("java/lang/Object"),
        interfaces: &[],
        access: PUBLIC,
        methods: natives![
            "getRuntime" "()Ljava/lang/Runtime;" S => Some(runtime_get),
            "availableProcessors" "()I" M => Some(|_, _| Ok(Value::Int(4))),
            "totalMemory" "()J" M => Some(|vm, _| Ok(Value::Long((vm.options.max_objects as i64) * 64))),
            "maxMemory" "()J" M => Some(|vm, _| Ok(Value::Long((vm.options.max_objects as i64) * 64))),
            "freeMemory" "()J" M => Some(|vm, _| Ok(Value::Long(((vm.options.max_objects as i64) - vm.objects.len() as i64) * 64))),
            "gc" "()V" M => Some(|_, _| void()),
            "exit" "(I)V" M => Some(|_, a| Err(Exc::Exit(arg(a, 1).i()))),
            "halt" "(I)V" M => Some(|_, a| Err(Exc::Exit(arg(a, 1).i()))),
            "addShutdownHook" "(Ljava/lang/Thread;)V" M => Some(|vm, a| { vm.jdk.threads.shutdown_hooks.push(arg(a, 1).r()); vm.pinned.push(arg(a, 1).r()); void() }),
            "removeShutdownHook" "(Ljava/lang/Thread;)Z" M => Some(|vm, a| { let r = arg(a, 1).r(); let before = vm.jdk.threads.shutdown_hooks.len(); vm.jdk.threads.shutdown_hooks.retain(|&h| h != r); ret_bool(before != vm.jdk.threads.shutdown_hooks.len()) }),
        ],
        statics: &[],
        clinit: None,
        data: None,
    },
    NativeClass {
        name: "java/lang/Throwable",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/io/Serializable"],
        access: PUBLIC,
        methods: THROWABLE_METHODS,
        statics: &[],
        clinit: None,
        data: Some(|| Data::Throwable(Box::default())),
    },
    NativeClass {
        name: "java/lang/StackTraceElement",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/io/Serializable"],
        access: PUBLIC | FINAL,
        methods: natives![
            "<init>" "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;I)V" M => Some(|vm, a| { let t = this(vm, a)?; set_generic(vm, t, a[1..].to_vec()); void() }),
            "getClassName" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; Ok(generic(vm, t, 0)) }),
            "getMethodName" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; Ok(generic(vm, t, 1)) }),
            "getFileName" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; Ok(generic(vm, t, 2)) }),
            "getLineNumber" "()I" M => Some(|vm, a| { let t = this(vm, a)?; Ok(generic(vm, t, 3)) }),
            "isNativeMethod" "()Z" M => Some(|vm, a| { let t = this(vm, a)?; ret_bool(generic(vm, t, 3).i() == -2) }),
            "getModuleName" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; Ok(generic(vm, t, 4)) }),
            "toString" "()Ljava/lang/String;" M => Some(stack_trace_element_to_string),
            "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| { let t = this(vm, a)?; let o = arg(a, 1).r(); let eq = o != 0 && (0..4).all(|i| generic_eq(vm, t, o, i)); ret_bool(eq) }),
            "hashCode" "()I" M => Some(|vm, a| { let t = this(vm, a)?; let s = vm.string(generic(vm, t, 0).r()).unwrap_or_default(); Ok(Value::Int(super::string::java_hash(&s.encode_utf16().collect::<Vec<_>>()).wrapping_mul(31).wrapping_add(generic(vm, t, 3).i()))) }),
        ],
        statics: &[],
        clinit: None,
        data: Some(|| Data::Jdk(Box::new(State::Generic(vec![Value::NULL, Value::NULL, Value::NULL, Value::Int(-1), Value::NULL])))),
    },
    NativeClass {
        name: "java/lang/Enum",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/lang/Comparable", "java/io/Serializable", "java/lang/constant/Constable"],
        access: PUBLIC | ABSTRACT,
        methods: natives![
            "<init>" "(Ljava/lang/String;I)V" 0x0004 => Some(|vm, a| { let t = this(vm, a)?; if let Some(o) = vm.obj_mut(t) { o.data = Data::Jdk(Box::new(State::Enum { name: arg(a, 1).r(), ordinal: arg(a, 2).i() })); } void() }),
            "name" "()Ljava/lang/String;" M | FINAL => Some(|vm, a| { let t = this(vm, a)?; Ok(Value::Ref(enum_parts(vm, t).0)) }),
            "ordinal" "()I" M | FINAL => Some(|vm, a| { let t = this(vm, a)?; Ok(Value::Int(enum_parts(vm, t).1)) }),
            "toString" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; Ok(Value::Ref(enum_parts(vm, t).0)) }),
            "equals" "(Ljava/lang/Object;)Z" M | FINAL => Some(|_, a| ret_bool(arg(a, 0).r() == arg(a, 1).r())),
            "hashCode" "()I" M | FINAL => Some(object_hash_code),
            "compareTo" "(Ljava/lang/Enum;)I" M | FINAL => Some(enum_compare_to),
            "compareTo" "(Ljava/lang/Object;)I" M => Some(enum_compare_to),
            "getDeclaringClass" "()Ljava/lang/Class;" M | FINAL => Some(|vm, a| { let t = this(vm, a)?; let c = enum_declaring_class(vm, t); ret_ref(vm.mirror(c)?) }),
            "valueOf" "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Enum;" S => Some(enum_value_of),
            "clone" "()Ljava/lang/Object;" 0x0004 => Some(|vm, _| Err(vm.throw_new("java/lang/CloneNotSupportedException", None))),
        ],
        statics: &[],
        clinit: None,
        data: Some(|| Data::Jdk(Box::new(State::Enum { name: 0, ordinal: 0 }))),
    },
    class("java/lang/Record", Some("java/lang/Object"), &[], PUBLIC | ABSTRACT, natives![
        "<init>" "()V" 0x0004 => Some(|_, _| void()),
        "equals" "(Ljava/lang/Object;)Z" A => None,
        "hashCode" "()I" A => None,
        "toString" "()Ljava/lang/String;" A => None,
    ]),
    NativeClass {
        name: "java/lang/Void",
        super_name: Some("java/lang/Object"),
        interfaces: &[],
        access: PUBLIC | FINAL,
        methods: &[],
        statics: &[("TYPE", "Ljava/lang/Class;")],
        clinit: Some(|vm, _| {
            let c = vm.load("java/lang/Void")?;
            let p = vm.primitive_class(b'V');
            let m = vm.mirror(p)?;
            vm.set_static(c, "TYPE", Value::Ref(m));
            void()
        }),
        data: None,
    },
    class("java/lang/SecurityManager", Some("java/lang/Object"), &[], PUBLIC, &[]),
    // The throwables: each takes Throwable's four constructors.
    class("java/lang/Exception", Some("java/lang/Throwable"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/Error", Some("java/lang/Throwable"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/RuntimeException", Some("java/lang/Exception"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/ArithmeticException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/ArrayStoreException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/ClassCastException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/IllegalArgumentException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/NumberFormatException", Some("java/lang/IllegalArgumentException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/IllegalStateException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/IllegalMonitorStateException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/IllegalCallerException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/IndexOutOfBoundsException", Some("java/lang/RuntimeException"), &[], PUBLIC, INDEX_CTORS),
    class("java/lang/ArrayIndexOutOfBoundsException", Some("java/lang/IndexOutOfBoundsException"), &[], PUBLIC, INDEX_CTORS),
    class("java/lang/StringIndexOutOfBoundsException", Some("java/lang/IndexOutOfBoundsException"), &[], PUBLIC, INDEX_CTORS),
    class("java/lang/NegativeArraySizeException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/NullPointerException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/UnsupportedOperationException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/SecurityException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/EnumConstantNotPresentException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/TypeNotPresentException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/LayerInstantiationException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/MatchException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/CloneNotSupportedException", Some("java/lang/Exception"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/InterruptedException", Some("java/lang/Exception"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/ReflectiveOperationException", Some("java/lang/Exception"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/ClassNotFoundException", Some("java/lang/ReflectiveOperationException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/InstantiationException", Some("java/lang/ReflectiveOperationException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/IllegalAccessException", Some("java/lang/ReflectiveOperationException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/NoSuchFieldException", Some("java/lang/ReflectiveOperationException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/NoSuchMethodException", Some("java/lang/ReflectiveOperationException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/LinkageError", Some("java/lang/Error"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/NoClassDefFoundError", Some("java/lang/LinkageError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/ClassFormatError", Some("java/lang/LinkageError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/UnsupportedClassVersionError", Some("java/lang/ClassFormatError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/ClassCircularityError", Some("java/lang/LinkageError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/VerifyError", Some("java/lang/LinkageError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/UnsatisfiedLinkError", Some("java/lang/LinkageError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/BootstrapMethodError", Some("java/lang/LinkageError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/IncompatibleClassChangeError", Some("java/lang/LinkageError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/AbstractMethodError", Some("java/lang/IncompatibleClassChangeError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/IllegalAccessError", Some("java/lang/IncompatibleClassChangeError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/InstantiationError", Some("java/lang/IncompatibleClassChangeError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/NoSuchFieldError", Some("java/lang/IncompatibleClassChangeError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/NoSuchMethodError", Some("java/lang/IncompatibleClassChangeError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/ExceptionInInitializerError", Some("java/lang/LinkageError"), &[], PUBLIC, natives![
        "<init>" "()V" M => Some(throwable_init),
        "<init>" "(Ljava/lang/String;)V" M => Some(throwable_init),
        "<init>" "(Ljava/lang/Throwable;)V" M => Some(|vm, a| { let t = this(vm, a)?; throwable_set(vm, t, 0, Some(arg(a, 1).r())); void() }),
        "getException" "()Ljava/lang/Throwable;" M => Some(throwable_get_cause),
    ]),
    class("java/lang/VirtualMachineError", Some("java/lang/Error"), &[], PUBLIC | ABSTRACT, THROWABLE_CTORS),
    class("java/lang/StackOverflowError", Some("java/lang/VirtualMachineError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/OutOfMemoryError", Some("java/lang/VirtualMachineError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/InternalError", Some("java/lang/VirtualMachineError"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/AssertionError", Some("java/lang/Error"), &[], PUBLIC, natives![
        "<init>" "()V" M => Some(throwable_init),
        "<init>" "(Ljava/lang/String;Ljava/lang/Throwable;)V" M => Some(throwable_init),
        "<init>" "(Ljava/lang/Object;)V" M => Some(assertion_error_init),
        "<init>" "(Z)V" M => Some(assertion_error_init),
        "<init>" "(C)V" M => Some(assertion_error_init),
        "<init>" "(I)V" M => Some(assertion_error_init),
        "<init>" "(J)V" M => Some(assertion_error_init),
        "<init>" "(F)V" M => Some(assertion_error_init),
        "<init>" "(D)V" M => Some(assertion_error_init),
    ]),
    class("java/lang/ThreadDeath", Some("java/lang/Error"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/NoSuchElementException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/ConcurrentModificationException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/EmptyStackException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/MissingResourceException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/IllegalFormatException", Some("java/lang/IllegalArgumentException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/UnknownFormatConversionException", Some("java/util/IllegalFormatException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/MissingFormatArgumentException", Some("java/util/IllegalFormatException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/IllegalFormatConversionException", Some("java/util/IllegalFormatException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/InputMismatchException", Some("java/util/NoSuchElementException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/concurrent/ExecutionException", Some("java/lang/Exception"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/concurrent/TimeoutException", Some("java/lang/Exception"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/concurrent/CancellationException", Some("java/lang/IllegalStateException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/concurrent/CompletionException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/concurrent/RejectedExecutionException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/concurrent/BrokenBarrierException", Some("java/lang/Exception"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/io/IOException", Some("java/lang/Exception"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/io/UncheckedIOException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/io/EOFException", Some("java/io/IOException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/io/FileNotFoundException", Some("java/io/IOException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/io/UnsupportedEncodingException", Some("java/io/IOException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/io/UTFDataFormatException", Some("java/io/IOException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/io/IOError", Some("java/lang/Error"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/nio/BufferUnderflowException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/nio/BufferOverflowException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/nio/charset/UnsupportedCharsetException", Some("java/lang/IllegalArgumentException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/reflect/InvocationTargetException", Some("java/lang/ReflectiveOperationException"), &[], PUBLIC, natives![
        "<init>" "()V" M => Some(throwable_init),
        "<init>" "(Ljava/lang/Throwable;)V" M => Some(|vm, a| { let t = this(vm, a)?; throwable_set(vm, t, 0, Some(arg(a, 1).r())); void() }),
        "<init>" "(Ljava/lang/Throwable;Ljava/lang/String;)V" M => Some(|vm, a| { let t = this(vm, a)?; throwable_set(vm, t, arg(a, 2).r(), Some(arg(a, 1).r())); void() }),
        "getTargetException" "()Ljava/lang/Throwable;" M => Some(throwable_get_cause),
    ]),
    class("java/lang/reflect/UndeclaredThrowableException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/lang/invoke/WrongMethodTypeException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/time/DateTimeException", Some("java/lang/RuntimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/time/format/DateTimeParseException", Some("java/time/DateTimeException"), &[], PUBLIC, THROWABLE_CTORS),
    class("java/util/regex/PatternSyntaxException", Some("java/lang/IllegalArgumentException"), &[], PUBLIC, THROWABLE_CTORS),
];

/// Throwable's constructors: every exception class gets these.
pub static THROWABLE_CTORS: &[super::NativeMethod] = natives![
    "<init>" "()V" M => Some(throwable_init),
    "<init>" "(Ljava/lang/String;)V" M => Some(throwable_init),
    "<init>" "(Ljava/lang/String;Ljava/lang/Throwable;)V" M => Some(throwable_init),
    "<init>" "(Ljava/lang/Throwable;)V" M => Some(throwable_init),
    "<init>" "(Ljava/lang/String;Ljava/lang/Throwable;ZZ)V" 0x0004 => Some(throwable_init),
];

/// `IndexOutOfBoundsException`'s constructors, its `(int)` and `(long)` ones included.
static INDEX_CTORS: &[super::NativeMethod] = natives![
    "<init>" "()V" M => Some(throwable_init),
    "<init>" "(Ljava/lang/String;)V" M => Some(throwable_init),
    "<init>" "(I)V" M => Some(index_init),
    "<init>" "(J)V" M => Some(index_init),
];

static THROWABLE_METHODS: &[super::NativeMethod] = natives![
    "<init>" "()V" M => Some(throwable_init),
    "<init>" "(Ljava/lang/String;)V" M => Some(throwable_init),
    "<init>" "(Ljava/lang/String;Ljava/lang/Throwable;)V" M => Some(throwable_init),
    "<init>" "(Ljava/lang/Throwable;)V" M => Some(throwable_init),
    "<init>" "(Ljava/lang/String;Ljava/lang/Throwable;ZZ)V" 0x0004 => Some(throwable_init),
    "getMessage" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; Ok(Value::Ref(throwable(vm, t).map_or(0, |d| d.message))) }),
    "getLocalizedMessage" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; vm.invoke_virtual(t, "getMessage", "()Ljava/lang/String;", &[]) }),
    "getCause" "()Ljava/lang/Throwable;" M => Some(throwable_get_cause),
    "initCause" "(Ljava/lang/Throwable;)Ljava/lang/Throwable;" M => Some(throwable_init_cause),
    "toString" "()Ljava/lang/String;" M => Some(throwable_to_string),
    "printStackTrace" "()V" M => Some(throwable_print_stack_trace),
    "printStackTrace" "(Ljava/io/PrintStream;)V" M => Some(throwable_print_stack_trace),
    "printStackTrace" "(Ljava/io/PrintWriter;)V" M => Some(throwable_print_stack_trace),
    "getStackTrace" "()[Ljava/lang/StackTraceElement;" M => Some(throwable_get_stack_trace),
    "setStackTrace" "([Ljava/lang/StackTraceElement;)V" M => Some(|vm, a| { let t = this(vm, a)?; let copy = vm.ref_elements(arg(a, 1).r()); let arr = vm.new_ref_array("Ljava/lang/StackTraceElement;", &copy)?; if let Some(Data::Throwable(d)) = vm.obj_mut(t).map(|o| &mut o.data) { d.trace = arr; } void() }),
    "fillInStackTrace" "()Ljava/lang/Throwable;" M => Some(|vm, a| { let t = this(vm, a)?; let frames = vm.stack_frames(); if let Some(Data::Throwable(d)) = vm.obj_mut(t).map(|o| &mut o.data) { d.frames = frames; d.trace = 0; } ret_ref(t) }),
    "addSuppressed" "(Ljava/lang/Throwable;)V" M => Some(throwable_add_suppressed),
    "getSuppressed" "()[Ljava/lang/Throwable;" M => Some(|vm, a| { let t = this(vm, a)?; let s = throwable(vm, t).map(|d| d.suppressed.clone()).unwrap_or_default(); ret_ref(vm.new_ref_array("Ljava/lang/Throwable;", &s)?) }),
];

// ------------------------------------------------------------------ Object

fn object_hash_code(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    Ok(Value::Int(vm.identity_hash(t)))
}

fn object_to_string(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let text = default_to_string(vm, t)?;
    ret_str(vm, &text)
}

/// Stores a reference into an array as `aastore` does: `ArrayStoreException`
/// for an element of the wrong class.
pub fn array_store(vm: &mut Vm, arr: ObjId, i: usize, v: Value) -> Res<()> {
    let Some(class) = vm.class_of(arr) else { return Err(vm.npe(None)) };
    if let ClassKind::Array { component, elem: b'L' } = vm.class(class).kind
        && v.r() != 0
        && !vm.instance_of(v.r(), component)
    {
        let name = vm.class_of(v.r()).map(|c| vm.class(c).java_name()).unwrap_or_default();
        return Err(vm.throw_new("java/lang/ArrayStoreException", Some(&name)));
    }
    let len = vm.array(arr).map_or(0, |a| a.len());
    if i >= len {
        let msg = format!("Index {i} out of bounds for length {len}");
        return Err(vm.throw_new("java/lang/ArrayIndexOutOfBoundsException", Some(&msg)));
    }
    if let Some(a) = vm.array_mut(arr) {
        a.set(i, v);
    }
    Ok(())
}

/// `Object.toString`: the class name, `@`, the hash in hex.
pub fn default_to_string(vm: &mut Vm, t: ObjId) -> Res<String> {
    let h = vm.invoke_virtual(t, "hashCode", "()I", &[])?.i();
    let name = vm.class_of(t).map(|c| vm.class(c).java_name()).unwrap_or_default();
    Ok(format!("{name}@{:x}", h as u32))
}

fn object_get_class(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let c = vm.class_of(t).unwrap_or(0);
    ret_ref(vm.mirror(c)?)
}

fn object_clone(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let o = vm.obj(t).cloned().ok_or_else(|| Exc::Internal("clone of a freed object".into()))?;
    let cloneable = vm.load("java/lang/Cloneable")?;
    if !vm.class(o.class).is_array() && !vm.is_subclass(o.class, cloneable) {
        let name = vm.class(o.class).java_name();
        return Err(vm.throw_new("java/lang/CloneNotSupportedException", Some(&name)));
    }
    let r = vm.alloc(Obj { hash: 0, ..o })?;
    ret_ref(r)
}

fn iterable_for_each(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let action = arg(a, 1).r();
    if action == 0 {
        return Err(vm.npe(None));
    }
    let it = vm.invoke_virtual(t, "iterator", "()Ljava/util/Iterator;", &[])?.r();
    while vm.invoke_virtual(it, "hasNext", "()Z", &[])?.z() {
        let v = vm.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])?;
        vm.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[v])?;
    }
    void()
}

// ------------------------------------------------------------------ System

fn system_clinit(vm: &mut Vm, _: &[Value]) -> Res<Value> {
    let c = vm.load("java/lang/System")?;
    let out = super::io::new_print_stream(vm, super::io::Sink::Stdout)?;
    let err = super::io::new_print_stream(vm, super::io::Sink::Stderr)?;
    let input = super::io::new_input_stream(vm, Vec::new())?;
    vm.set_static(c, "out", Value::Ref(out));
    vm.set_static(c, "err", Value::Ref(err));
    vm.set_static(c, "in", Value::Ref(input));
    if vm.jdk.properties.is_empty() {
        let mut props: Vec<(String, String)> = [
            ("java.version", "25.0.5"),
            ("java.specification.version", "25"),
            ("java.vendor", "Horizon"),
            ("java.vm.name", "Horizon Java VM"),
            ("os.name", "Linux"),
            ("os.arch", "amd64"),
            ("file.separator", "/"),
            ("path.separator", ":"),
            ("line.separator", "\n"),
            ("user.language", "en"),
            ("user.dir", "/"),
            ("file.encoding", "UTF-8"),
            ("native.encoding", "UTF-8"),
            ("stdout.encoding", "UTF-8"),
            ("stderr.encoding", "UTF-8"),
            ("java.io.tmpdir", "/tmp"),
            ("java.class.version", "69.0"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in vm.options.properties.clone() {
            props.retain(|(pk, _)| *pk != k);
            props.push((k, v));
        }
        vm.jdk.properties = props;
    }
    void()
}

fn system_get_property(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let key = vm.string_nn(arg(a, 0).r())?;
    if key.is_empty() {
        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("key can't be empty")));
    }
    match vm.jdk.properties.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()) {
        Some(v) => ret_str(vm, &v),
        None => Ok(Value::Ref(if a.len() > 1 { arg(a, 1).r() } else { 0 })),
    }
}

fn system_set_property(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let key = vm.string_nn(arg(a, 0).r())?;
    let value = vm.string_nn(arg(a, 1).r())?;
    let old = vm.jdk.properties.iter().position(|(k, _)| *k == key);
    let previous = old.map(|i| vm.jdk.properties.remove(i).1);
    vm.jdk.properties.push((key, value));
    match previous {
        Some(p) => ret_str(vm, &p),
        None => ret_ref(0),
    }
}

fn system_clear_property(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let key = vm.string_nn(arg(a, 0).r())?;
    match vm.jdk.properties.iter().position(|(k, _)| *k == key) {
        Some(i) => {
            let v = vm.jdk.properties.remove(i).1;
            ret_str(vm, &v)
        }
        None => ret_ref(0),
    }
}

fn system_arraycopy(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let (src, sp, dst, dp, len) = (arg(a, 0).r(), arg(a, 1).i(), arg(a, 2).r(), arg(a, 3).i(), arg(a, 4).i());
    if src == 0 || dst == 0 {
        return Err(vm.npe(None));
    }
    let (Some(s), Some(d)) = (vm.array(src), vm.array(dst)) else {
        let which = if vm.array(src).is_none() { src } else { dst };
        let name = vm.class_of(which).map(|c| vm.class(c).java_name()).unwrap_or_default();
        let msg = format!("arraycopy: {} type {name} is not an array", if which == src { "source" } else { "destination" });
        return Err(vm.throw_new("java/lang/ArrayStoreException", Some(&msg)));
    };
    let (sl, dl) = (s.len() as i64, d.len() as i64);
    if std::mem::discriminant(s) != std::mem::discriminant(d) {
        let (sc, dc) = (vm.class_of(src).unwrap(), vm.class_of(dst).unwrap());
        let msg = format!("arraycopy: type mismatch: can not copy {} into {}", vm.class(sc).java_name().replace("[", "").replace(';', ""), vm.class(dc).java_name());
        let _ = msg;
        let msg = format!("arraycopy: type mismatch: can not copy {}[] into {}[]", component_name(vm, sc), component_name(vm, dc));
        return Err(vm.throw_new("java/lang/ArrayStoreException", Some(&msg)));
    }
    if sp < 0 || dp < 0 || len < 0 || sp as i64 + len as i64 > sl || dp as i64 + len as i64 > dl {
        let msg = if len < 0 {
            format!("arraycopy: length {len} is negative")
        } else if sp < 0 || sp as i64 + len as i64 > sl {
            if sp < 0 {
                format!("arraycopy: source index {sp} out of bounds for {}[{sl}]", component_name(vm, vm.class_of(src).unwrap()))
            } else {
                format!("arraycopy: last source index {} out of bounds for {}[{sl}]", sp as i64 + len as i64, component_name(vm, vm.class_of(src).unwrap()))
            }
        } else if dp < 0 {
            format!("arraycopy: destination index {dp} out of bounds for {}[{dl}]", component_name(vm, vm.class_of(dst).unwrap()))
        } else {
            format!("arraycopy: last destination index {} out of bounds for {}[{dl}]", dp as i64 + len as i64, component_name(vm, vm.class_of(dst).unwrap()))
        };
        return Err(vm.throw_new("java/lang/ArrayIndexOutOfBoundsException", Some(&msg)));
    }
    let (sp, dp, len) = (sp as usize, dp as usize, len as usize);
    // Reference arrays check each element against the destination's type.
    if let Some(Array::L(items)) = vm.array(src) {
        let items: Vec<ObjId> = items[sp..sp + len].to_vec();
        let dc = vm.class_of(dst).unwrap();
        let comp = match vm.class(dc).kind {
            ClassKind::Array { component, .. } => component,
            _ => 0,
        };
        for (i, &r) in items.iter().enumerate() {
            if r != 0 && !vm.instance_of(r, comp) {
                let msg = format!("arraycopy: element type mismatch: can not cast one of the elements of {}[] to the type of the destination array, {}", vm.class(vm.class_of(src).unwrap()).java_name().trim_start_matches("[L").trim_end_matches(';'), vm.class(comp).java_name());
                // The elements before the one that fails are copied, as HotSpot's are.
                if let Some(Array::L(d)) = vm.array_mut(dst) {
                    d[dp..dp + i].copy_from_slice(&items[..i]);
                }
                return Err(vm.throw_new("java/lang/ArrayStoreException", Some(&msg)));
            }
        }
        if let Some(Array::L(d)) = vm.array_mut(dst) {
            d[dp..dp + len].copy_from_slice(&items);
        }
        return void();
    }
    let s = vm.array(src).unwrap().clone();
    let d = vm.array_mut(dst).unwrap();
    macro_rules! copy {
        ($($v:ident),*) => {
            match (&s, d) {
                $( (Array::$v(s), Array::$v(d)) => d[dp..dp + len].copy_from_slice(&s[sp..sp + len]), )*
                _ => {}
            }
        };
    }
    copy!(Z, B, C, S, I, J, F, D);
    void()
}

fn component_name(vm: &Vm, array: ClassId) -> String {
    match vm.class(array).kind {
        ClassKind::Array { elem: b'L', .. } => "object array".to_string(),
        ClassKind::Array { component, .. } => vm.class(component).java_name(),
        _ => vm.class(array).java_name(),
    }
}

fn runtime_get(vm: &mut Vm, _: &[Value]) -> Res<Value> {
    if vm.jdk.threads.runtime == 0 {
        let c = vm.load("java/lang/Runtime")?;
        let r = vm.new_instance(c)?;
        vm.pinned.push(r);
        vm.jdk.threads.runtime = r;
    }
    ret_ref(vm.jdk.threads.runtime)
}

// --------------------------------------------------------------- Throwable

pub fn throwable(vm: &Vm, r: ObjId) -> Option<&ThrowableData> {
    match &vm.obj(r)?.data {
        Data::Throwable(t) => Some(t),
        _ => None,
    }
}

fn throwable_set(vm: &mut Vm, t: ObjId, message: ObjId, cause: Option<ObjId>) {
    let frames = throwable_frames(vm, t);
    if let Some(o) = vm.obj_mut(t) {
        if !matches!(o.data, Data::Throwable(_)) {
            o.data = Data::Throwable(Box::default());
        }
        if let Data::Throwable(d) = &mut o.data {
            d.message = message;
            if cause.is_some() {
                d.cause = cause;
            }
            d.frames = frames;
            d.trace = 0;
        }
    }
}

/// The frames a new throwable records: the current ones less the
/// constructors of the throwable itself (HotSpot does not show them).
fn throwable_frames(vm: &Vm, t: ObjId) -> Vec<(MethodId, u32)> {
    let class = vm.class_of(t).unwrap_or(0);
    let mut frames = vm.stack_frames();
    while let Some(&(m, _)) = frames.first() {
        let method = vm.method(m);
        if &*method.name == "<init>" && vm.is_subclass(class, method.class) {
            frames.remove(0);
        } else {
            break;
        }
    }
    frames
}

fn throwable_init(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let desc_args = &a[1..];
    let throwable_class = vm.known.throwable;
    let (mut message, mut cause) = (0, None);
    match desc_args {
        [] => {}
        [Value::Ref(m)] => {
            // `(String)` or `(Throwable)`: a throwable argument is the cause,
            // and its toString() the message.
            if *m != 0 && vm.instance_of(*m, throwable_class) && vm.string_units(*m).is_none() {
                cause = Some(*m);
                let text = vm.invoke_virtual(*m, "toString", "()Ljava/lang/String;", &[])?;
                message = text.r();
            } else if *m == 0 && a.len() == 2 && is_cause_ctor(vm, t) {
                cause = Some(0);
            } else {
                message = *m;
            }
        }
        [Value::Ref(m), Value::Ref(c), ..] => {
            message = *m;
            cause = Some(*c);
        }
        _ => {}
    }
    throwable_set(vm, t, message, cause);
    void()
}

/// Whether the constructor running is a `(Throwable)` one (only needed to
/// tell `new X((Throwable) null)` from `new X((String) null)`).
fn is_cause_ctor(vm: &Vm, _t: ObjId) -> bool {
    let Some(f) = vm.frames.last() else {
        return false;
    };
    let d = vm.method(f.method).decoded.as_ref();
    let Some(d) = d else {
        return false;
    };
    let at = f.pc.saturating_sub(1) as usize;
    if let Some(Op::InvokeDirect(m)) = d.ops.get(at) {
        return vm.method(*m).desc.contains("Throwable;)V");
    }
    false
}

fn index_init(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let index = match arg(a, 1) {
        Value::Long(l) => l,
        v => v.i() as i64,
    };
    let msg = vm.new_string(&format!("Index out of range: {index}"))?;
    throwable_set(vm, t, msg, None);
    void()
}

fn assertion_error_init(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let v = arg(a, 1);
    let desc = vm.frames.last().and_then(|f| {
        vm.method(f.method).decoded.as_ref().and_then(|d| match d.ops.get(f.pc.saturating_sub(1) as usize) {
            Some(Op::InvokeDirect(m)) => Some(vm.method(*m).desc.clone()),
            _ => None,
        })
    });
    let param = desc
        .as_deref()
        .and_then(|d| d.strip_prefix('('))
        .and_then(|d| d.strip_suffix(")V"))
        .unwrap_or("Ljava/lang/Object;")
        .to_string();
    let text = vm.stringify(v, &param)?;
    let msg = vm.new_string_units(text)?;
    let throwable_class = vm.known.throwable;
    let cause = (param.starts_with('L') && v.r() != 0 && vm.instance_of(v.r(), throwable_class)).then_some(v.r());
    throwable_set(vm, t, msg, cause);
    void()
}

fn throwable_get_cause(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    Ok(Value::Ref(throwable(vm, t).and_then(|d| d.cause).unwrap_or(0)))
}

fn throwable_init_cause(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let cause = arg(a, 1).r();
    let current = throwable(vm, t).map(|d| d.cause);
    if let Some(Some(existing)) = current {
        let text = if existing == 0 { "a null".to_string() } else { vm.throwable_text(existing) };
        let me = vm.throwable_text(t);
        let msg = format!("Can't overwrite cause with {} for {me}", if cause == 0 { "a null".to_string() } else { vm.throwable_text(cause) });
        let _ = text;
        return Err(vm.throw_new("java/lang/IllegalStateException", Some(&msg)));
    }
    if cause == t {
        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Self-causation not permitted")));
    }
    if let Some(Data::Throwable(d)) = vm.obj_mut(t).map(|o| &mut o.data) {
        d.cause = Some(cause);
    }
    ret_ref(t)
}

fn throwable_to_string(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let name = vm.class_of(t).map(|c| vm.class(c).java_name()).unwrap_or_default();
    let msg = vm.invoke_virtual(t, "getLocalizedMessage", "()Ljava/lang/String;", &[])?.r();
    match vm.string(msg) {
        Some(m) => ret_str(vm, &format!("{name}: {m}")),
        None => ret_str(vm, &name),
    }
}

fn throwable_add_suppressed(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let s = arg(a, 1).r();
    if s == t {
        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Self-suppression not permitted")));
    }
    if s == 0 {
        return Err(vm.npe(Some("Cannot suppress a null exception.")));
    }
    if let Some(Data::Throwable(d)) = vm.obj_mut(t).map(|o| &mut o.data) {
        d.suppressed.push(s);
    }
    void()
}

/// One frame as `StackTraceElement` parts: class, method, file, line.
pub fn frame_parts(vm: &Vm, m: MethodId, at: u32) -> (String, String, Option<String>, i32) {
    let method = vm.method(m);
    let class = vm.class(method.class);
    let file = class.class_file().and_then(|cf| cf.source_file.as_ref().map(|s| s.to_string()));
    let line = match &method.body {
        crate::vm::MethodBody::Bytecode(code) => {
            let pc = method
                .decoded
                .as_ref()
                .and_then(|d| d.pcs.get(at as usize).copied())
                .unwrap_or(0);
            crate::vm::classfile::ClassFile::line_of(code, pc as usize).map_or(-1, |l| l as i32)
        }
        crate::vm::MethodBody::Native(_) => -2,
        _ => -1,
    };
    (class.java_name(), method.name.to_string(), file, line)
}

fn throwable_get_stack_trace(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let existing = throwable(vm, t).map_or(0, |d| d.trace);
    let trace = if existing != 0 {
        existing
    } else {
        let frames = throwable(vm, t).map(|d| d.frames.clone()).unwrap_or_default();
        let ste = vm.load("java/lang/StackTraceElement")?;
        let mut items = Vec::new();
        for (m, at) in frames {
            if vm.method(m).decoded.is_none() && !matches!(vm.method(m).body, crate::vm::MethodBody::Bytecode(_)) {
                continue;
            }
            let (c, n, f, l) = frame_parts(vm, m, at);
            let e = vm.new_instance(ste)?;
            let (c, n) = (vm.new_string(&c)?, vm.new_string(&n)?);
            let f = match f {
                Some(f) => vm.new_string(&f)?,
                None => 0,
            };
            set_generic(vm, e, vec![Value::Ref(c), Value::Ref(n), Value::Ref(f), Value::Int(l), Value::NULL]);
            items.push(e);
        }
        let arr = vm.new_ref_array("Ljava/lang/StackTraceElement;", &items)?;
        if let Some(Data::Throwable(d)) = vm.obj_mut(t).map(|o| &mut o.data) {
            d.trace = arr;
        }
        arr
    };
    let copy = vm.ref_elements(trace);
    ret_ref(vm.new_ref_array("Ljava/lang/StackTraceElement;", &copy)?)
}

/// The `\tat ...` lines of a throwable's trace, its causes' too, as
/// `printStackTrace` prints them.
pub fn trace_lines(vm: &mut Vm, ex: ObjId) -> String {
    let mut out = String::new();
    let mut seen = vec![ex];
    let mut current = ex;
    let mut first = true;
    loop {
        if !first {
            let text = vm.throwable_text(current);
            out.push_str(&format!("Caused by: {text}\n"));
        }
        first = false;
        let frames = throwable(vm, current).map(|d| d.frames.clone()).unwrap_or_default();
        for (m, at) in frames {
            if !matches!(vm.method(m).body, crate::vm::MethodBody::Bytecode(_)) {
                continue;
            }
            let (c, n, f, l) = frame_parts(vm, m, at);
            out.push_str(&format!("\tat {}\n", element_text(&c, &n, f.as_deref(), l)));
        }
        let cause = throwable(vm, current).and_then(|d| d.cause).unwrap_or(0);
        if cause == 0 || seen.contains(&cause) {
            break;
        }
        seen.push(cause);
        current = cause;
    }
    out
}

fn element_text(class: &str, method: &str, file: Option<&str>, line: i32) -> String {
    let location = match (file, line) {
        (_, -2) => "Native Method".to_string(),
        (Some(f), l) if l >= 0 => format!("{f}:{l}"),
        (Some(f), _) => f.to_string(),
        (None, _) => "Unknown Source".to_string(),
    };
    format!("{class}.{method}({location})")
}

fn throwable_print_stack_trace(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let text = vm.throwable_text(t);
    let mut s = format!("{text}\n");
    s.push_str(&trace_lines(vm, t));
    let suppressed = throwable(vm, t).map(|d| d.suppressed.clone()).unwrap_or_default();
    for sup in suppressed {
        let st = vm.throwable_text(sup);
        s.push_str(&format!("\tSuppressed: {st}\n"));
    }
    match a.get(1) {
        Some(Value::Ref(stream)) if *stream != 0 => {
            let r = vm.new_string(&s)?;
            vm.invoke_virtual(*stream, "print", "(Ljava/lang/String;)V", &[Value::Ref(r)])?;
        }
        _ => vm.stderr.extend_from_slice(s.as_bytes()),
    }
    void()
}

fn set_generic(vm: &mut Vm, r: ObjId, values: Vec<Value>) {
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(values)));
    }
}

fn generic(vm: &Vm, r: ObjId, i: usize) -> Value {
    match state(vm, r) {
        Some(State::Generic(v)) => v.get(i).copied().unwrap_or_default(),
        _ => Value::NULL,
    }
}

fn generic_eq(vm: &Vm, a: ObjId, b: ObjId, i: usize) -> bool {
    let (x, y) = (generic(vm, a, i), generic(vm, b, i));
    match (x, y) {
        (Value::Ref(p), Value::Ref(q)) => vm.string(p) == vm.string(q),
        _ => x == y,
    }
}

fn stack_trace_element_to_string(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let c = vm.string(generic(vm, t, 0).r()).unwrap_or_default();
    let n = vm.string(generic(vm, t, 1).r()).unwrap_or_default();
    let f = vm.string(generic(vm, t, 2).r());
    let l = generic(vm, t, 3).i();
    ret_str(vm, &element_text(&c, &n, f.as_deref(), l))
}

// ------------------------------------------------------------------ Enum

pub fn enum_parts(vm: &Vm, r: ObjId) -> (ObjId, i32) {
    match state(vm, r) {
        Some(State::Enum { name, ordinal }) => (*name, *ordinal),
        _ => (0, 0),
    }
}

pub fn enum_name(vm: &Vm, r: ObjId) -> Option<String> {
    vm.string(enum_parts(vm, r).0)
}

fn enum_declaring_class(vm: &Vm, r: ObjId) -> ClassId {
    let c = vm.class_of(r).unwrap_or(0);
    // A constant with a body is an anonymous subclass of its enum.
    match vm.class(c).super_class {
        Some(s) if vm.class(s).access & ACC_ENUM != 0 && &*vm.class(s).name != "java/lang/Enum" => s,
        _ => c,
    }
}

fn enum_compare_to(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let o = arg(a, 1).r();
    if o == 0 {
        return Err(vm.npe(None));
    }
    if enum_declaring_class(vm, t) != enum_declaring_class(vm, o) {
        return Err(vm.throw_new("java/lang/ClassCastException", None));
    }
    Ok(Value::Int(enum_parts(vm, t).1 - enum_parts(vm, o).1))
}

/// An enum class's constants, in declaration order (`values()`).
pub fn enum_constants(vm: &mut Vm, c: ClassId) -> Res<Vec<ObjId>> {
    vm.ensure_init(c)?;
    let Some(m) = vm.find_method(c, "values", &format!("()[L{};", vm.class(c).name)) else {
        return Ok(Vec::new());
    };
    let arr = vm.invoke(m, &[])?.r();
    Ok(vm.ref_elements(arr))
}

fn enum_value_of(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let Some(c) = vm.mirror_class(arg(a, 0).r()) else {
        return Err(vm.npe(Some("Name is null")));
    };
    let name = arg(a, 1).r();
    if name == 0 {
        return Err(vm.npe(Some("Name is null")));
    }
    let wanted = vm.string(name).unwrap_or_default();
    for e in enum_constants(vm, c)? {
        if enum_name(vm, e).as_deref() == Some(&wanted) {
            return ret_ref(e);
        }
    }
    let msg = format!("No enum constant {}.{wanted}", vm.class(c).java_name().replace('$', "."));
    Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)))
}

// ------------------------------------------------------------------ helpers

/// `Class.getSimpleName()`.
pub fn simple_name(vm: &Vm, c: ClassId) -> String {
    let class = vm.class(c);
    match class.kind {
        ClassKind::Array { component, .. } => return simple_name(vm, component) + "[]",
        ClassKind::Primitive(p) => return crate::vm::class::primitive_name(p).to_string(),
        _ => {}
    }
    if let Some(cf) = class.class_file() {
        for ic in &cf.inner_classes {
            if cf.class_name(ic.inner).ok().as_deref() == Some(&*class.name) {
                return if ic.name == 0 {
                    String::new()
                } else {
                    cf.utf8(ic.name).map(|u| u.text.to_string()).unwrap_or_default()
                };
            }
        }
    }
    let name = class.name.rsplit('/').next().unwrap_or("");
    if matches!(class.kind, ClassKind::Synthetic) {
        // A lambda class keeps its hidden name, past the outer class.
        return name.to_string();
    }
    name.to_string()
}

/// `Objects.equals`: identity, then `a.equals(b)`.
pub fn objects_equals(vm: &mut Vm, a: ObjId, b: ObjId) -> Res<bool> {
    if a == b {
        return Ok(true);
    }
    if a == 0 || b == 0 {
        return Ok(false);
    }
    if let (Some(x), Some(y)) = (vm.string_units(a), vm.string_units(b)) {
        return Ok(x == y);
    }
    Ok(vm.invoke_virtual(a, "equals", "(Ljava/lang/Object;)Z", &[Value::Ref(b)])?.z())
}

/// `Objects.hashCode` of a reference (0 for null), fast for strings and wrappers.
pub fn object_hash(vm: &mut Vm, r: ObjId) -> Res<i32> {
    if r == 0 {
        return Ok(0);
    }
    if let Some(Data::Str(units, h)) = vm.obj(r).map(|o| &o.data) {
        if *h != 0 {
            return Ok(*h);
        }
        let h = super::string::java_hash(units);
        if let Some(Data::Str(_, cache)) = vm.obj_mut(r).map(|o| &mut o.data) {
            *cache = h;
        }
        return Ok(h);
    }
    Ok(vm.invoke_virtual(r, "hashCode", "()I", &[])?.i())
}

/// The hash a record component of descriptor `desc` contributes.
pub fn hash_of(vm: &mut Vm, v: Value, desc: &str) -> Res<i32> {
    Ok(match desc.as_bytes().first() {
        Some(b'Z') => {
            if v.i() != 0 {
                1231
            } else {
                1237
            }
        }
        Some(b'B' | b'C' | b'S' | b'I') => v.i(),
        Some(b'J') => (v.j() ^ (v.j() >> 32)) as i32,
        Some(b'F') => super::boxes::float_to_int_bits(v.f()),
        Some(b'D') => {
            let bits = super::boxes::double_to_long_bits(v.d());
            (bits ^ (bits >> 32)) as i32
        }
        _ => object_hash(vm, v.r())?,
    })
}

/// A primitive boxed through its wrapper's `valueOf` (the caches included).
pub fn box_value(vm: &mut Vm, v: Value, kind: u8) -> Res<ObjId> {
    super::boxes::box_value(vm, v, kind)
}

/// A wrapper unboxed to the primitive `kind`, widening as Java's unboxing
/// conversions do; `NullPointerException` for null.
pub fn unbox(vm: &mut Vm, r: ObjId, kind: u8) -> Res<Value> {
    if r == 0 {
        return Err(vm.npe(None));
    }
    let v = match vm.obj(r).map(|o| &o.data) {
        Some(Data::Boxed(v)) => *v,
        _ => {
            let name = vm.class_of(r).map(|c| vm.class(c).java_name()).unwrap_or_default();
            let target = match kind {
                b'Z' => "java.lang.Boolean",
                b'C' => "java.lang.Character",
                b'J' => "java.lang.Long",
                b'F' => "java.lang.Float",
                b'D' => "java.lang.Double",
                _ => "java.lang.Integer",
            };
            let msg = format!("class {name} cannot be cast to class {target}");
            return Err(vm.throw_new("java/lang/ClassCastException", Some(&msg)));
        }
    };
    Ok(widen(v, kind))
}

/// A primitive widened (or narrowed, for a sub-int) to `kind`.
pub fn widen(v: Value, kind: u8) -> Value {
    match (kind, v) {
        (b'J', Value::Int(i)) => Value::Long(i as i64),
        (b'F', Value::Int(i)) => Value::Float(i as f32),
        (b'D', Value::Int(i)) => Value::Double(i as f64),
        (b'F', Value::Long(l)) => Value::Float(l as f32),
        (b'D', Value::Long(l)) => Value::Double(l as f64),
        (b'D', Value::Float(f)) => Value::Double(f as f64),
        (b'I' | b'S' | b'B' | b'C' | b'Z', Value::Long(l)) => Value::Int(l as i32),
        (_, v) => v,
    }
}

/// JEP 358's helpful message for a `NullPointerException` at instruction
/// `at` of `m`: what the instruction did, and where the null came from.
pub fn helpful_npe(vm: &Vm, m: MethodId, at: usize) -> Option<String> {
    let method = vm.method(m);
    let d = method.decoded.as_ref()?;
    let op = d.ops.get(at)?;
    let cf = &d.cf;
    let code = &d.code;
    let bytes = &code.code;
    let pc = *d.pcs.get(at)? as usize;
    let opcode = *bytes.get(pc)?;
    let action = match opcode {
        0xb6 | 0xb9 | 0xb7 => {
            let index = u16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]);
            let (class, name, desc, _) = cf.member_ref(index).ok()?;
            format!("Cannot invoke \"{}\"", method_text(&class, &name, &desc))
        }
        0xb4 => {
            let index = u16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]);
            let (_, name, _, _) = cf.member_ref(index).ok()?;
            format!("Cannot read field \"{name}\"")
        }
        0xb5 => {
            let index = u16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]);
            let (_, name, _, _) = cf.member_ref(index).ok()?;
            format!("Cannot assign field \"{name}\"")
        }
        0xbe => "Cannot read the array length".to_string(),
        0x2e..=0x35 => format!("Cannot load from {} array", array_kind(opcode - 0x2e)),
        0x4f..=0x56 => format!("Cannot store to {} array", array_kind(opcode - 0x4f)),
        0xbf => "Cannot throw exception".to_string(),
        0xc2 => "Cannot enter synchronized block".to_string(),
        0xc3 => "Cannot exit synchronized block".to_string(),
        _ => return None,
    };
    let _ = op;
    // The instruction that pushed the null: the receiver's slot below the arguments.
    let depth = match opcode {
        0xb6 | 0xb9 | 0xb7 => {
            let index = u16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]);
            let (_, _, desc, _) = cf.member_ref(index).ok()?;
            let (params, _) = crate::vm::classfile::parse_method_desc(&desc)?;
            params.len()
        }
        0xb5 => 1,
        0x2e..=0x35 => 1,
        0x4f..=0x56 => 2,
        _ => 0,
    };
    let source = null_source(vm, m, at, depth);
    Some(match source {
        Some(s) => format!("{action} because \"{s}\" is null"),
        None => action,
    })
}

fn array_kind(n: u8) -> &'static str {
    match n {
        0 => "int",
        1 => "long",
        2 => "float",
        3 => "double",
        4 => "object",
        5 => "byte/boolean",
        6 => "char",
        _ => "short",
    }
}

fn method_text(class: &str, name: &str, desc: &str) -> String {
    let params = crate::vm::classfile::parse_method_desc(desc)
        .map(|(p, _)| p.iter().map(|d| short_class(&crate::vm::class::desc_to_java(d))).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    format!("{}.{name}({params})", short_class(class))
}

fn short_class(class: &str) -> String {
    let java = class.replace('/', ".");
    match java.strip_prefix("java.lang.") {
        Some(rest) if !rest.contains('.') => rest.to_string(),
        _ => java,
    }
}

/// Where the value `depth` slots below the top of the stack at instruction
/// `at` came from, walking back over the simple instructions that pushed it.
fn null_source(vm: &Vm, m: MethodId, at: usize, depth: usize) -> Option<String> {
    let d = vm.method(m).decoded.as_ref()?;
    let bytes = &d.code.code;
    let mut need = depth;
    let mut i = at;
    while i > 0 {
        i -= 1;
        let pc = d.pcs[i] as usize;
        let opcode = bytes[pc];
        let (pops, pushes) = stack_effect(&d.cf, bytes, pc)?;
        if need < pushes {
            return Some(describe_push(vm, m, i)?);
        }
        need = need - pushes + pops;
        if matches!(opcode, 0xa7 | 0xc8 | 0xaa | 0xab | 0xac..=0xb1 | 0xbf) {
            return None;
        }
    }
    None
}

fn describe_push(vm: &Vm, m: MethodId, i: usize) -> Option<String> {
    let d = vm.method(m).decoded.as_ref()?;
    let bytes = &d.code.code;
    let pc = d.pcs[i] as usize;
    let opcode = bytes[pc];
    let local = |slot: usize| -> String {
        let method = vm.method(m);
        if slot == 0 && !method.is_static() {
            return "this".to_string();
        }
        d.code
            .locals
            .iter()
            .find(|(start, len, _, s)| *s as usize == slot && (*start as usize) <= pc + 2 && pc < (*start as usize + *len as usize + 3))
            .map(|(_, _, n, _)| n.to_string())
            .unwrap_or_else(|| format!("<local{slot}>"))
    };
    match opcode {
        0x19 => Some(local(bytes[pc + 1] as usize)),
        0x2a..=0x2d => Some(local((opcode - 0x2a) as usize)),
        0x01 => Some("null".to_string()),
        0xb2 => {
            let index = u16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]);
            let (class, name, _, _) = d.cf.member_ref(index).ok()?;
            Some(format!("{}.{name}", short_class(&class).rsplit('.').next().unwrap_or("")))
        }
        0xb4 => {
            let index = u16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]);
            let (_, name, _, _) = d.cf.member_ref(index).ok()?;
            let from = describe_push_below(vm, m, i).unwrap_or_default();
            Some(if from.is_empty() { name.to_string() } else { format!("{from}.{name}") })
        }
        0xb6 | 0xb7 | 0xb8 | 0xb9 => {
            let index = u16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]);
            let (class, name, desc, _) = d.cf.member_ref(index).ok()?;
            Some(format!("the return value of \"{}\"", method_text(&class, &name, &desc)))
        }
        0x32 => {
            let array = describe_at_depth(vm, m, i, 1).unwrap_or_else(|| "...".into());
            let index = describe_at_depth(vm, m, i, 0).unwrap_or_else(|| "...".into());
            Some(format!("{array}[{index}]"))
        }
        0x15 => Some(local(bytes[pc + 1] as usize)),
        0x1a..=0x1d => Some(local((opcode - 0x1a) as usize)),
        _ => None,
    }
}

fn describe_push_below(vm: &Vm, m: MethodId, i: usize) -> Option<String> {
    describe_at_depth(vm, m, i, 0)
}

fn describe_at_depth(vm: &Vm, m: MethodId, i: usize, depth: usize) -> Option<String> {
    let d = vm.method(m).decoded.as_ref()?;
    let mut need = depth;
    let mut j = i;
    while j > 0 {
        j -= 1;
        let pc = d.pcs[j] as usize;
        let (pops, pushes) = stack_effect(&d.cf, &d.code.code, pc)?;
        if need < pushes {
            return describe_push(vm, m, j);
        }
        need = need - pushes + pops;
    }
    None
}

/// Values (not slots) an instruction pops and pushes, for the instructions
/// [`null_source`] walks back over.
fn stack_effect(cf: &crate::vm::classfile::ClassFile, bytes: &[u8], pc: usize) -> Option<(usize, usize)> {
    let opcode = *bytes.get(pc)?;
    Some(match opcode {
        0x00 => (0, 0),
        0x01..=0x14 => (0, 1),
        0x15..=0x2d => (0, 1),
        0x2e..=0x35 => (2, 1),
        0x36..=0x4e => (1, 0),
        0x4f..=0x56 => (3, 0),
        0x57 => (1, 0),
        0x59 => (1, 2),
        0x60..=0x73 | 0x78..=0x83 => (2, 1),
        0x74..=0x77 | 0x85..=0x93 => (1, 1),
        0x84 => (0, 0),
        0x94..=0x98 => (2, 1),
        0xb2 => (0, 1),
        0xb3 => (1, 0),
        0xb4 => (1, 1),
        0xb5 => (2, 0),
        0xb6..=0xb9 => {
            let index = u16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]);
            let (_, _, desc, _) = cf.member_ref(index).ok()?;
            let (params, ret) = crate::vm::classfile::parse_method_desc(&desc)?;
            (params.len() + (opcode != 0xb8) as usize, (ret != "V") as usize)
        }
        0xbb => (0, 1),
        0xbc | 0xbd => (1, 1),
        0xbe => (1, 1),
        0xc0 | 0xc1 => (1, 1),
        _ => return None,
    })
}
