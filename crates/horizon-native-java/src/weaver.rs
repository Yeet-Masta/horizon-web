//! A bounded class-file writer and injection weaver.
//!
//! The interpreter in [`crate`] never needed to write a class: it reads a
//! `ClassFile` and executes a bytecode subset.  This module adds the other
//! direction, for the shapes a native Mixin translation can prepare before an
//! embedded JVM sees them: a method that gets a `HEAD` callback at offset 0, or
//! one that gets a callback immediately before each `return`.
//!
//! [`ClassFile::write`] re-encodes a parsed class from the bytes it was parsed from.
//! Every byte range the parser does not need to understand (the constant pool
//! payloads, fields, unknown attributes, trailing bytes) is copied verbatim, so
//! an untouched class round-trips byte-for-byte.  Constant pool entries the
//! writer appends are the only bytes it invents.
//!
//! # What the weaver supports
//!
//! [`weave_head`] inserts, at offset 0 of the target method:
//!
//! ```text
//! aload_0                         (only when the callback takes the receiver)
//! <load every argument from its local slot>
//! new CallbackInfo; dup; invokespecial <init>()V   (only with a trailing CallbackInfo)
//! invokestatic callback
//! ```
//!
//! The `CallbackInfo` is built after the arguments so it is the last value the
//! callback consumes, and it lives only on the operand stack: no fresh local,
//! no `max_locals` growth and no verifier frame changes.
//!
//! [`weave_return`] inserts a callback immediately before every return-family
//! instruction (`return`, `ireturn`, `areturn`, `freturn`, `dreturn`,
//! `lreturn`) for [`ReturnMode::Return`], or before only the last one for
//! [`ReturnMode::Tail`] (Mixin's `TAIL`).  The callback carries the target's
//! arguments, plus the receiver when it declares it.
//!
//! A `void` target weaves the plain `invokestatic`.  A non-`void` target also
//! carries its result: the callback's last argument is a
//! `CallbackInfoReturnable`, which the insert builds around the target's boxed
//! value - the same class [`callback_info_returnable_class`] adds to the jar:
//!
//! ```text
//! invokestatic  java/lang/Integer.valueOf                (primitive returns only)
//! astore        <fresh local>
//! new           org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable
//! dup
//! aload         <fresh local>
//! invokespecial CallbackInfoReturnable.<init>(Ljava/lang/Object;)V
//! astore        <fresh local>
//! <load every callback argument: the receiver, then the target's arguments>
//! aload         <fresh local>
//! invokestatic  callback
//! aload         <fresh local>
//! invokevirtual CallbackInfoReturnable.getReturnValue()Ljava/lang/Object;
//! checkcast     <wrapper for a primitive, the return type otherwise>
//! invokevirtual <wrapper>.<type>Value                    (primitive returns only)
//! ```
//!
//! The object is built before the arguments because the callback consumes the
//! topmost one as its last argument, and the second `aload` keeps the woven
//! method's own reference for `getReturnValue`.
//!
//! The object starts with the target's own boxed result, so a callback that
//! never calls `setReturnValue` changes nothing and the last `setReturnValue`
//! wins.  A `cancel()` keeps the override, because the method returns
//! `getReturnValue()` either way - which is what Mixin's own `RETURN` handler
//! does for a cancellable non-void injection.  The insert therefore adds no
//! branch, and the fresh local is the only verifier state it touches.
//!
//! Because an insert moves every byte after it, both anchors relocate everything
//! in the method that names a bytecode offset, each by the delta that applies at
//! its own offset (a `RETURN` insert only moves the bytes after that `return`):
//!
//! - 16-bit branch offsets (`if*`, `goto`, `jsr`, `ifnull`, `ifnonnull`) and the
//!   32-bit `goto_w`/`jsr_w`, keeping each target on the instruction it named
//!   (or on the insert in front of it, per the `RETURN` rule below);
//! - the exception table's `start_pc`, `end_pc` and `handler_pc`: the inserted
//!   callback is never caught by the target's own handler, because an insert at
//!   a range start lands before it and an insert at the exclusive end lands
//!   after it;
//! - `LineNumberTable`, `LocalVariableTable` and `LocalVariableTypeTable`
//!   `start_pc` (and the local range's end, so its length grows with the code);
//! - `StackMapTable` frame offsets, re-derived from the shifted absolute
//!   offsets as cumulative `offset_delta`s, plus the `uninitialized` offsets
//!   inside frames.  A frame whose delta no longer fits its compact form is
//!   widened (`same_frame` to `same_frame_extended`, and so on).
//! - the `default` and case offsets of a `tableswitch`/`lookupswitch`, and the
//!   alignment padding its own new offset forces.  The padding is 0..=3 bytes,
//!   so relocating a switch can move the bytes after it a second time, on top
//!   of the insert.
//!
//! A relocation is therefore a layout pass, not a single splice: the pass walks
//! the original instructions left to right, places the inserted bytes at the
//! instruction they precede, and gives each switch the padding its *new* offset
//! asks for.  Padding depends only on that offset, which the pass has already
//! fixed, so one pass is self-consistent.  The pass repeats - bounded by
//! `MAX_RELOCATION_PASSES` - only when a `goto`/`jsr` has to grow to
//! `goto_w`/`jsr_w` because an insert pushed its target past the 16-bit range;
//! growing an instruction moves everything after it, so the next pass has to
//! agree.  A layout that does not settle inside the bound is rejected.
//!
//! A conditional branch (`if*`, `ifnull`, `ifnonnull`) has no wide encoding, so
//! one whose relocated target no longer fits 16 bits is split in place:
//!
//! ```text
//! if<inverse> +8       (skips the wide jump when the original condition is false)
//! goto_w target
//! <the original fallthrough instruction>
//! ```
//!
//! The fallthrough instruction becomes a branch target and therefore needs a
//! `StackMapTable` frame.  The frame's locals are derived by following the
//! straight-line region from the nearest preceding frame (or the method entry)
//! with a bounded type model - local loads and stores, constants, fields,
//! calls, arrays, `checkcast` and the stack shuffles - so the frame is exact
//! rather than guessed; an instruction the model cannot follow is a reason, not
//! a guessed frame.  When such a frame is added, every frame in the method is
//! re-encoded as a `full_frame`, because a later `append`/`chop` frame's delta
//! is relative to the frame in front of it.
//!
//! An insert sits in front of the instruction it precedes, and two details
//! follow from that.  A `RETURN`/`TAIL` insert is *entered*: a branch or switch
//! case that named the `return` now lands on the callback, so a `return` that is
//! also a jump target - the body of a `switch` case, an `if` arm - still calls
//! back.  A `HEAD` insert is *skipped*: a backward branch to the method's first
//! instruction lands after the callback, so the callback runs once at entry.
//! When an insert lands on a `StackMapTable` frame position the frame is emitted
//! before the callback, because the bytes after an unconditional transfer start
//! a basic block whether or not anything branches to them and a `HEAD` or `void`
//! callback runs in the same state.  Such an insert leaves stack and locals
//! untouched, so the frame is emitted at the instruction too; a non-void
//! `RETURN` insert writes a fresh local, so only the insert keeps the frame and
//! the `return` behind it - which now starts no basic block - carries none.
//!
//! `wide`-prefixed local instructions (`wide` load/store, `iinc`, `ret`) are
//! walked at their real length and copied verbatim, so a method whose local
//! slots exceed 255 weaves exactly like one below the limit.
//!
//! `max_stack` is raised conservatively (HEAD: `original + argument slots + 1`;
//! RETURN/TAIL: `original + argument slots`, plus three slots for the
//! `CallbackInfoReturnable` construction of a non-void target), `max_locals` is
//! raised to the argument slots when the class declares fewer (and by one for
//! the fresh local of a non-void insert), and the `code_length` and `Code`
//! attribute length follow the rewritten code.  Attributes the weaver does not
//! read (`Exceptions`, type annotations, ...) are still copied byte-for-byte.
//!
//! The callback descriptor must consume exactly the target's arguments and
//! return `void`.  An instance target's callback may instead take the target
//! instance as an extra first parameter, which the inserted `aload_0` supplies.
//! A non-cancellable callback may end with a plain `CallbackInfo`, which the
//! insert constructs and passes; nothing reads `isCancelled` on it, so a
//! `cancel()` from a non-cancellable callback changes nothing.  A non-void
//! `RETURN`/`TAIL` callback must end with a `CallbackInfoReturnable`, which the
//! insert supplies.  A cancellable `void` `HEAD` callback ends with a
//! `CallbackInfo` instead: the insert constructs [`callback_info_class`], passes
//! it, and emits `if (!ci.isCancelled()) goto body; return`, so a cancelled
//! callback skips the target body and the method answers with the `void`
//! default.
//!
//! [`weave_class`] applies every matching `HEAD` plan first, then every
//! `RETURN`/`TAIL` plan, re-parsing the class between edits and skipping plans
//! for other classes.  [`weave_classes`] groups plans by target owner, weaves
//! each affected class with [`weave_class`], and re-encodes every other class
//! unchanged.  [`weave_classes_with_extras`] additionally reports the synthetic
//! classes a rewrite references ([`WovenClasses::added`]), which is what a jar
//! writer has to add beside the rewritten class.
//!
//! A callback that is an *instance* method of its owner cannot be the target of
//! an `invokestatic`.  [`weave_classes_with_extras`] detects those from the
//! class set and routes them through one synthetic static bridge class per
//! (target, callback owner) pair: the woven insert still calls `invokestatic`,
//! the bridge holds the instance in a static field and delegates with
//! `invokevirtual`.  See [`weave_classes_with_extras_and_instance_callbacks`]
//! for the explicit form a caller with a partial class set uses.
//!
//! # What it does not support
//!
//! [`weave_plans`] applies the matching plain `HEAD` plans, in plan order (plus
//! a selected `HEAD` ordinal), and skips plans that target another class or ask
//! for cancellation; [`weave_class`] applies `HEAD`, `RETURN`/`TAIL` and
//! `INVOKE` plans instead, and rejects an anchor it cannot weave and a
//! per-class plan overflow with a reason.
//!
//! `@At("INVOKE")` is also available on its own through [`weave_invoke`] (and
//! [`weave_invoke_plan`]); [`weave_class`] applies the same edit as one group
//! after the around-the-body plans, re-parsing between passes so a call-site
//! hook and a return hook on one method agree.  The bounded shape is a `void` or value-returning `invokevirtual`/`invokestatic`/
//! `invokeinterface` for a non-cancellable callback; cancellation also needs a
//! `void` call and exactly one matching instruction, because a value would have
//! to be replaced by the callee descriptor's default.  `invokespecial` call
//! sites stay on the interpreter path.  A cancellable `RETURN`/`TAIL` on a
//! `void` target, and a cancellable `HEAD` on a non-void one, stay there too:
//! the first has no result to carry, the second needs the descriptor's default
//! value in the target's shape.  A non-void `RETURN`/`TAIL` callback that does
//! not declare the trailing `CallbackInfoReturnable` cannot observe the result
//! and stays there as well.
//!
//! The synthetic `CallbackInfoReturnable` is the smallest class that carries the
//! result, so it does not extend Mixin's `CallbackInfo`: a callback that casts
//! it to `CallbackInfo`, or that asks either synthetic class for `isCancellable`,
//! is outside the bounded subset.  A jar that already declares the class keeps
//! its own copy.
//!
//! # Extended anchors and injectors
//!
//! [`weave_anchor`] takes an [`AnchorRequest`] instead of one of the fixed
//! plans: an `@At("FIELD")`, `NEW` or `JUMP` site, the `INVOKE` selector, or
//! `HEAD`, with `ordinal`, `@Slice`, a signed `shift`/`by` distance and an
//! `@Inject`/`@Redirect` choice.  [`AnchorRequest::from_plan`] reads discovery's
//! [`MixinAnchorPlan`].
//!
//! - `@Inject` at `FIELD`, `NEW` or `JUMP` inserts the callback immediately
//!   before the selected instruction.  A field *write* whose callback declares
//!   the field value gets it as the callback's last argument: the insert stores
//!   the value in a fresh local, pushes it after the target's arguments, calls
//!   the callback and reloads it for the write.  A write callback that does not
//!   declare it runs above the field operands the write already pushed.
//! - `@Redirect` at `FIELD` replaces a read with the callback's return value
//!   (the receiver is popped first) and a write with a callback that takes the
//!   target arguments followed by the value and returns `void`.  `@Redirect` at
//!   `INVOKE` saves the call's arguments in fresh locals, pops the receiver and
//!   calls the callback with the target's arguments; the original call becomes
//!   `nop`s.  A `@Redirect` at `HEAD` is the whole-body [`weave_overwrite`].
//! - [`weave_overwrite`] replaces a method body with one `invokestatic` and the
//!   matching `xreturn`.  The original exception table, debug tables and
//!   stack-map frames are dropped with the body; a constructor or static
//!   initializer is rejected because it still has to call `super`.
//! - [`weave_modify_arg`] rewrites one argument of a selected call.  A simple
//!   callback takes that argument alone; the insert saves the arguments above
//!   it in fresh locals, calls the callback and loads them back.  A full
//!   callback mirrors the entire argument list; the insert saves every
//!   argument and reloads the list with the result in place of `index`.
//! - [`weave_modify_variable`] hooks a `STORE` (before the store) or a `LOAD`
//!   (after the load) of a local selected by absolute slot or by the nth access
//!   of the callback's value type.  A value-only callback is a stack-neutral
//!   `invokestatic`; a callback that also captures the target arguments uses a
//!   fresh local.
//!
//! Fresh locals are declared in every later `StackMapTable` frame (the full
//! frame encoding is forced so a compressed delta cannot disagree).  A shifted
//! insert walks the same instruction-boundary bound as [`ShiftBy`], and a
//! redirect cannot be shifted because its replacement bytes have to stay on
//! the instruction they replace.
//!
//! [`weave_class_extended`] applies every family to one class, and
//! [`weave_extended_classes`] to a class set.  Instance callbacks from *any*
//! family are routed through the same synthetic bridge class (the bridge emits
//! the callback's own `xreturn`, so a non-void callback works too).
//!
//! What remains on the interpreter path, and why:
//!
//! - `@Redirect` at `NEW`: replacing `new`/`dup`/`<init>` changes object
//!   initialization, not an insert.  A bounded rewrite would have to remove the
//!   original constructor call and leave no uninitialized reference behind.
//! - `@Redirect` at `JUMP`: replacing a branch rewrites control flow; no
//!   stack-neutral insert expresses it.
//! - `@Redirect` at `invokespecial` (a constructor or private call): the
//!   receiver may be an uninitialized object, so saving it in a local and
//!   replacing the call would not verify.
//! - A cancellable `@Inject` at `FIELD`/`NEW`/`JUMP`: cancellation needs a
//!   branch and a value in the target's shape, which those anchors do not have.
//! - `@ModifyVariable` at `LOAD` with a captured argument list when a later
//!   stack-map frame exists: a branch that skips the load reaches the frame
//!   without the insert's fresh local, so one frame cannot describe both paths.
//! - An `@Overwrite` whose callback takes `CallbackInfo`: a cancellable
//!   overwrite has to answer with the descriptor's default value, which the
//!   body-replacement shape does not construct.
//! - `@At(shift)`/`by` on FIELD/NEW/JUMP is available through
//!   [`AnchorRequest::shift`], but discovery's [`MixinAnchorPlan`] has no field
//!   for it yet.
//!
//! # Selector modifiers
//!
//! [`weave_invoke_at`], [`weave_invoke_selected`], [`weave_invoke_shifted`] and
//! [`weave_invoke_shifted_by`] model Mixin's `ordinal`, `slice`, `shift` and
//! `by` on an `@At("INVOKE")` anchor; [`weave_returns_selected`],
//! [`weave_returns_shifted`], [`weave_head_selected`] and [`weave_head_shifted`]
//! model the same modifiers on `RETURN`/`TAIL`/`HEAD`.  [`InvokeSelector`] names
//! a call by owner, name and descriptor (an empty descriptor matches every
//! overload), keeps the matches inside an optional [`SliceBounds`], and then
//! the `ordinal`-th one - 0-based, negative from the end (`-1` is the last).  A
//! slice boundary is another [`SiteSelector`]: `HEAD`, the first `RETURN` or
//! `TAIL`, a `RETURN`/`TAIL` by ordinal (`ReturnOrdinal`/`TailOrdinal`), or one
//! call instruction - the plain form requires exactly one match, while
//! `InvokeOrdinal` names the `ordinal`-th matching call.  A boundary that names
//! more than one instruction, a slice that ends before it starts, and an
//! ordinal outside the matches are reasons, not guesses.
//! [`InvokeShift`] moves the insert `by` instruction boundaries (bounded to
//! `0..=8`) before or after the selected site; [`Shift::By`] moves it a signed
//! `-8..=8` boundaries from it.  An insert that would land past the method or
//! inside an exception range is a reason, and a cancellable insert cannot be
//! shifted because its cancellation branch has to sit on the call.
//! [`weave_returns_selected`] takes `non_void` to say the callback observes the
//! result through `CallbackInfoReturnable`, which the target's descriptor has to
//! confirm; the plain callback shape it otherwise emits is stack-neutral, so a
//! non-void return accepts it too.
//!
//! # `CONSTANT_Dynamic`
//!
//! A class whose constant pool already uses `CONSTANT_Dynamic` (condy) is
//! preserved, not rejected - including a class that also carries javac's
//! `invokedynamic` string-concatenation bootstrap.  The writer appends new
//! constants after the original entries, so no existing index moves, and it
//! re-encodes the pool and `BootstrapMethods` through an index map so the same
//! guarantee holds if a caller ever compacts the pool.  Before weaving, every
//! condy entry is validated as a coherent shape: the name/type is a field
//! descriptor, the bootstrap index names a `BootstrapMethods` row, that row's
//! handle is a legal `CONSTANT_MethodHandle`, and every static argument is a
//! loadable constant, recursively for another condy.  The bootstrap method is
//! never evaluated; only its references are preserved.  A malformed entry that
//! names the reserved index 0 keeps the historical blanket rejection, because
//! there is no coherent condy shape to relocate.
//!
//! Non-ASCII constant pool strings and code that would exceed the class-file
//! `code_length` limit are rejected with a reason rather than written.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    CALLBACK_INFO_CLASS, CALLBACK_INFO_DESC, CALLBACK_INFO_RETURNABLE_CLASS,
    CALLBACK_INFO_RETURNABLE_DESC, ClassFile, Cp, MethodRef, MixinAnchorPlan, MixinInjectionPlan,
    MixinModifyArgPlan, MixinModifyVariablePlan,
};

const ACC_STATIC: u16 = 0x0008;
const ACC_ABSTRACT: u16 = 0x0400;
const ACC_NATIVE: u16 = 0x0100;
const ACC_PUBLIC: u16 = 0x0001;
const ACC_PRIVATE: u16 = 0x0002;
/// `ACC_SUPER`, which every class file since Java 1.0.2 sets so `invokespecial`
/// dispatches through the superclass.
const ACC_SUPER: u16 = 0x0020;
/// The class-file version of the synthetic classes this module writes: Java 8,
/// which every JVM that can load a mod's classes accepts.
const SYNTHETIC_CLASS_MAJOR: u16 = 52;
const MAX_CODE_LENGTH: usize = 65_535;
/// Relocation passes before a layout has to be stable.  A pass only ever grows
/// an instruction (`goto`/`jsr` to `goto_w`/`jsr_w`), never shrinks one, so the
/// layout is monotone and settles in at most one pass per widening round plus
/// one to confirm; the bound keeps a pathological method bounded instead of
/// looping, and a method that hits it is rejected rather than written.
const MAX_RELOCATION_PASSES: usize = 16;
/// Plans [`weave_class`] applies to one class.  A jar with more matching plans
/// than this is outside the bounded weaver instead of growing the edit loop.
const MAX_WEAVE_PLANS_PER_CLASS: usize = 32;
/// Conditional branches one method may split.  Each split walks the straight-line
/// region to derive a frame, so a method that needs more is outside the bounded
/// weaver rather than a quadratic derivation.
const MAX_EXPANDED_BRANCHES: usize = 64;

/// Which return instruction a [`weave_return`] insert lands before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnMode {
    /// Before every return-family instruction (`RETURN`).
    Return,
    /// Before the last return-family instruction only (`TAIL`).
    Tail,
}

impl ReturnMode {
    fn anchor(self) -> &'static str {
        match self {
            Self::Return => "RETURN",
            Self::Tail => "TAIL",
        }
    }
}

/// Instructions an [`InvokeShift`] may move an insert by.  The bound keeps the
/// shifted site a few instructions from the selected one instead of a scan.
/// Discovery shares it so a malformed `@At.by` stays report-only.
pub(crate) const MAX_SHIFT_BY: u8 = 8;

/// The signed [`ShiftBy`] bound on either side of the selected site.
const MAX_SIGNED_SHIFT: i32 = MAX_SHIFT_BY as i32;

/// One bounded instruction selector: what a [`SliceBounds`] boundary names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteSelector {
    /// A call instruction by owner, name and descriptor.  An empty descriptor
    /// matches every overload of the name, like [`InvokeSelector`].
    Invoke {
        owner: String,
        name: String,
        descriptor: String,
    },
    /// The `ordinal`-th matching call instruction, 0-based and negative from
    /// the end (`-1` is the last).  The ordinal is resolved against every
    /// matching call before the slice is applied, so a boundary can name a
    /// site the slice itself does not otherwise count.
    InvokeOrdinal {
        owner: String,
        name: String,
        descriptor: String,
        ordinal: i32,
    },
    /// The method's first instruction.
    Head,
    /// The method's first return-family instruction.
    Return,
    /// The `ordinal`-th return-family instruction, 0-based and negative from
    /// the end (`-1` is the last), which is the ordinal [`Self::Return`] and
    /// [`Self::Tail`] leave implicit.
    ReturnOrdinal { ordinal: i32 },
    /// The method's last return-family instruction.
    Tail,
    /// The `ordinal`-th return-family instruction counted the [`Self::Return`]
    /// way; `-1` is the last, which is exactly what plain `TAIL` names.
    TailOrdinal { ordinal: i32 },
}

impl SiteSelector {
    /// The anchor name an ordinal error names.
    fn anchor(&self) -> &'static str {
        match self {
            Self::Invoke { .. } | Self::InvokeOrdinal { .. } => "INVOKE",
            Self::Head => "HEAD",
            Self::Return | Self::ReturnOrdinal { .. } => "RETURN",
            Self::Tail | Self::TailOrdinal { .. } => "TAIL",
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Invoke {
                owner,
                name,
                descriptor,
            } => format!("INVOKE {owner}.{name}{descriptor}"),
            Self::InvokeOrdinal {
                owner,
                name,
                descriptor,
                ordinal,
            } => format!("INVOKE {owner}.{name}{descriptor} ordinal {ordinal}"),
            Self::Head => "HEAD".to_owned(),
            Self::Return => "RETURN".to_owned(),
            Self::ReturnOrdinal { ordinal } => format!("RETURN ordinal {ordinal}"),
            Self::Tail => "TAIL".to_owned(),
            Self::TailOrdinal { ordinal } => format!("TAIL ordinal {ordinal}"),
        }
    }
}

/// The bounded slice an ordinal counts inside: `from` is inclusive and `to` is
/// exclusive, the way Mixin's `slice = @Slice(from = ..., to = ...)` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceBounds {
    pub from: SiteSelector,
    pub to: SiteSelector,
}

/// A bounded `@At(value = "INVOKE", ordinal = ..., slice = ...)` selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvokeSelector {
    pub owner: String,
    pub name: String,
    /// Empty matches every overload of `name`.
    pub descriptor: String,
    /// `None` keeps every matching site; `Some(n)` keeps the 0-based `n`-th,
    /// negative from the end (`-1` is the last).
    pub ordinal: Option<i32>,
    /// Which of the method's call sites are counted.
    pub slice: Option<SliceBounds>,
}

/// Which side of the selected site an [`InvokeShift`] moves the insert to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shift {
    /// Before the `by`-th instruction in front of the selected site.
    Before,
    /// After the `by`-th instruction after the selected site.
    After,
}

/// A bounded `@At(shift = ..., by = ...)`: how far from the selected call site
/// the callback runs, in instruction boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvokeShift {
    pub direction: Shift,
    pub by: u8,
}

impl InvokeShift {
    /// `0` names the instruction next to the site; more than [`MAX_SHIFT_BY`] is
    /// a reason instead of a walk.
    fn distance(self, label: &str) -> Result<usize, String> {
        if self.by > MAX_SHIFT_BY {
            return Err(format!(
                "{label}: shift by {} is outside the bounded shift of 0..={MAX_SHIFT_BY} instruction(s)",
                self.by
            ));
        }
        Ok(self.by as usize)
    }
}

/// A signed `@At.Shift.BY` distance: positive moves the insert towards the end
/// of the method, negative towards its start, and `0` names the selected site
/// itself.  [`MAX_SHIFT_BY`] bounds the magnitude on either side.
///
/// Mixin spells the shift `BY` and takes its amount from `@At.by`, which is a
/// signed `int`; [`Shift::By`] builds this value.  [`InvokeShift`] keeps the
/// `Before`/`After` direction and an unsigned amount, so its existing callers
/// are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShiftBy {
    pub by: i32,
}

impl ShiftBy {
    fn distance(self, label: &str) -> Result<i32, String> {
        if self.by < -MAX_SIGNED_SHIFT || self.by > MAX_SIGNED_SHIFT {
            return Err(format!(
                "{label}: shift BY by {} is outside the bounded shift of -{MAX_SIGNED_SHIFT}..={MAX_SIGNED_SHIFT} instruction(s)",
                self.by
            ));
        }
        Ok(self.by)
    }
}

impl Shift {
    /// Mixin's `@At.Shift.BY`: move the insert `by` instruction boundaries,
    /// positive towards the end of the method and negative towards its start.
    ///
    /// The variant keeps Mixin's spelling while the bounded [`Shift`] enum
    /// stays the two directions every exhaustive `match` over it names; the
    /// signed entry points ([`weave_invoke_shifted_by`],
    /// [`weave_returns_shifted`], [`weave_head_shifted`]) take the [`ShiftBy`]
    /// this builds.
    #[allow(non_snake_case)]
    pub const fn By(by: i32) -> ShiftBy {
        ShiftBy { by }
    }
}

/// The shift an `INVOKE` insert applies, before the signed distance is known.
#[derive(Debug, Clone, Copy)]
enum AnyShift {
    Before(u8),
    After(u8),
    By(i32),
}

impl AnyShift {
    /// The signed instruction distance the layout walks from the selected
    /// site.  `Before by 0` and `After by 0` keep their historical meaning: the
    /// site itself and the instruction after it.
    fn delta(self, label: &str) -> Result<i32, String> {
        match self {
            Self::Before(by) => {
                InvokeShift {
                    direction: Shift::Before,
                    by,
                }
                .distance(label)?;
                Ok(-i32::from(by))
            }
            Self::After(by) => {
                InvokeShift {
                    direction: Shift::After,
                    by,
                }
                .distance(label)?;
                Ok(i32::from(by) + 1)
            }
            Self::By(by) => ShiftBy { by }.distance(label),
        }
    }
}

impl From<InvokeShift> for AnyShift {
    fn from(shift: InvokeShift) -> Self {
        match shift.direction {
            Shift::Before => Self::Before(shift.by),
            Shift::After => Self::After(shift.by),
        }
    }
}

/// The bounded site selector [`weave_returns_selected`] uses.
#[derive(Debug, Clone)]
pub struct ReturnSelector {
    /// `RETURN` counts every return inside the slice; `TAIL` keeps the last one.
    pub mode: ReturnMode,
    /// `None` keeps every site the mode selects; `Some(n)` keeps the 0-based
    /// `n`-th, negative from the end (`-1` is the last).
    pub ordinal: Option<i32>,
    /// The return instructions the ordinal counts inside.
    pub slice: Option<SliceBounds>,
}

/// Which field access an `@At("FIELD")` anchor names.
///
/// A selector without an opcode keeps [`Self::Any`]: it matches every read or
/// write of the field, the way Mixin's `FIELD` anchor does when a mod does not
/// pin the access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldAccess {
    Get,
    Put,
    StaticGet,
    StaticPut,
    Any,
}

impl FieldAccess {
    fn of_opcode(opcode: u8) -> Option<Self> {
        Some(match opcode {
            0xb2 => Self::StaticGet,
            0xb3 => Self::StaticPut,
            0xb4 => Self::Get,
            0xb5 => Self::Put,
            _ => return None,
        })
    }

    fn is_put(self) -> bool {
        matches!(self, Self::Put | Self::StaticPut)
    }

    fn is_static(self) -> bool {
        matches!(self, Self::StaticGet | Self::StaticPut)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Get => "GETFIELD",
            Self::Put => "PUTFIELD",
            Self::StaticGet => "GETSTATIC",
            Self::StaticPut => "PUTSTATIC",
            Self::Any => "FIELD",
        }
    }
}

/// One `@At("FIELD")` site: the access kind and the field it names.
///
/// `owner` and `descriptor` are optional for the same reason
/// [`InvokeSelector`]'s descriptor is: an omitted owner matches the field on
/// any type, and an omitted descriptor matches every field of that name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSite {
    pub access: FieldAccess,
    pub owner: Option<String>,
    pub name: String,
    pub descriptor: Option<String>,
}

impl FieldSite {
    fn matches(&self, access: FieldAccess, owner: &str, name: &str, descriptor: &str) -> bool {
        (self.access == FieldAccess::Any || self.access == access)
            && self
                .owner
                .as_deref()
                .is_none_or(|expected| expected == owner)
            && self.name == name
            && self
                .descriptor
                .as_deref()
                .is_none_or(|expected| expected == descriptor)
    }
}

/// One `@At("NEW")` site: the internal name of the constructed class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSite {
    pub owner: String,
}

/// One `@At("JUMP")` site: an optional jump opcode and bytecode offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpSite {
    pub opcode: Option<u8>,
    pub offset: Option<usize>,
}

/// The instruction site an [`AnchorRequest`] names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorSite {
    Field(FieldSite),
    New(NewSite),
    Jump(JumpSite),
    Invoke(InvokeSelector),
    Head,
}

impl AnchorSite {
    fn label(&self) -> String {
        match self {
            Self::Field(site) => site.access.label().to_owned(),
            Self::New(site) => format!("NEW {}", site.owner),
            Self::Jump(site) => match (site.opcode, site.offset) {
                (Some(opcode), Some(offset)) => {
                    format!("JUMP {} at {offset}", opcode_name(opcode))
                }
                (Some(opcode), None) => format!("JUMP {}", opcode_name(opcode)),
                (None, Some(offset)) => format!("JUMP at {offset}"),
                (None, None) => "JUMP".to_owned(),
            },
            Self::Invoke(selector) => format!(
                "INVOKE {}.{}{}",
                selector.owner, selector.name, selector.descriptor
            ),
            Self::Head => "HEAD".to_owned(),
        }
    }
}

/// One bounded anchor rewrite: the site, its ordinal/slice/shift modifiers,
/// and whether it is an `@Inject` (callback before the site) or a `@Redirect`
/// (callback replaces the site).
///
/// [`AnchorRequest::from_plan`] builds one from discovery's
/// [`MixinAnchorPlan`].  The plan keeps `shift`/`by` out of its fields today, so
/// a caller that has them sets [`Self::shift`] directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorRequest {
    pub site: AnchorSite,
    /// `@At.ordinal` over the matching sites, negative from the end.
    pub ordinal: Option<i32>,
    /// `@Slice(from = ..., to = ...)`, a half-open instruction range.
    pub slice: Option<SliceBounds>,
    /// `@At(shift = ..., by = ...)`, a signed instruction distance from the
    /// selected site.  Redirects cannot be shifted: their replacement bytes
    /// have to stay on the instruction they replace.
    pub shift: Option<ShiftBy>,
    /// `@Redirect` instead of `@Inject`.
    pub redirect: bool,
    pub cancellable: bool,
}

impl AnchorRequest {
    /// Read discovery's raw anchor plan.
    ///
    /// A `FIELD` selector keeps its opcode when it names one of the four access
    /// instructions; a `NEW` selector names an internal class; a `JUMP`
    /// selector carries its bounded opcode and numeric offset; `HEAD` is only
    /// meaningful for a `@Redirect`, where it is the whole-body replacement
    /// [`weave_overwrite`] emits.
    pub fn from_plan(plan: &MixinAnchorPlan) -> Result<Self, String> {
        let site = match plan.at.as_str() {
            "HEAD" => AnchorSite::Head,
            "FIELD" => {
                let spec = plan.site.as_deref().ok_or_else(|| {
                    "@At(\"FIELD\") has no target selector; native field anchors need owner/name/descriptor"
                        .to_owned()
                })?;
                let mut site = parse_field_site(spec).ok_or_else(|| {
                    format!(
                        "@At(\"FIELD\") selector {spec:?} is not one of the bounded owner/name/descriptor forms"
                    )
                })?;
                site.access = match plan.opcode {
                    Some(opcode) => FieldAccess::of_opcode(opcode).ok_or_else(|| {
                        format!(
                            "@At(\"FIELD\") opcode {opcode} is not GETFIELD, PUTFIELD, GETSTATIC or PUTSTATIC"
                        )
                    })?,
                    None => site.access,
                };
                AnchorSite::Field(site)
            }
            "NEW" => {
                let spec = plan.site.as_deref().ok_or_else(|| {
                    "@At(\"NEW\") has no target class; native construction anchors need a class descriptor"
                        .to_owned()
                })?;
                AnchorSite::New(parse_new_site(spec).ok_or_else(|| {
                    format!("@At(\"NEW\") selector {spec:?} is not a bounded internal class name")
                })?)
            }
            "JUMP" => {
                if plan
                    .opcode
                    .is_some_and(|opcode| !is_bounded_jump_opcode(opcode))
                {
                    return Err(format!(
                        "@At(\"JUMP\") opcode {} is not a bounded conditional/unconditional jump",
                        plan.opcode.unwrap_or_default()
                    ));
                }
                let offset = match plan.site.as_deref() {
                    None => None,
                    Some(spec) => {
                        let numeric = spec
                            .rsplit_once(':')
                            .map_or(spec, |(_, offset)| offset)
                            .trim();
                        Some(numeric.parse::<usize>().map_err(|_| {
                            format!(
                                "@At(\"JUMP\") selector {spec:?} is not a numeric bytecode offset"
                            )
                        })?)
                    }
                };
                AnchorSite::Jump(JumpSite {
                    opcode: plan.opcode,
                    offset,
                })
            }
            "INVOKE" => {
                let spec = plan
                    .site
                    .as_deref()
                    .ok_or_else(|| "@At(\"INVOKE\") has no target selector".to_owned())?;
                let site = parse_invoke_site(spec).ok_or_else(|| {
                    format!(
                        "@At(\"INVOKE\") selector {spec:?} is not one of the bounded call-site forms"
                    )
                })?;
                AnchorSite::Invoke(InvokeSelector {
                    owner: site.owner,
                    name: site.name,
                    descriptor: site.descriptor,
                    ordinal: None,
                    slice: None,
                })
            }
            other => {
                return Err(format!(
                    "native anchor weaving does not support anchor {other}; it applies HEAD, INVOKE, FIELD, NEW and JUMP"
                ));
            }
        };
        let slice = anchor_slice_bounds(plan)?;
        Ok(Self {
            site,
            ordinal: plan.ordinal,
            slice,
            shift: None,
            redirect: plan.redirect,
            cancellable: plan.cancellable,
        })
    }

    /// The request an [`InvokeSelector`] plus its modifiers describes.
    pub fn invoke(
        selector: InvokeSelector,
        ordinal: Option<i32>,
        slice: Option<SliceBounds>,
        shift: Option<ShiftBy>,
        cancellable: bool,
    ) -> Self {
        Self {
            site: AnchorSite::Invoke(selector),
            ordinal,
            slice,
            shift,
            redirect: false,
            cancellable,
        }
    }
}

/// Parse `@At("FIELD")`'s selector, accepting the forms Mixin configs write:
/// `Lowner/Type;name:desc`, `owner/Type#name:desc`, `owner.Type.name:desc`, the
/// owner-less `name`/`name:desc`/`#name`, and any of those without a descriptor.
fn parse_field_site(spec: &str) -> Option<FieldSite> {
    let spec = spec.trim();
    if spec.is_empty() || spec.contains(char::is_whitespace) {
        return None;
    }
    let (head, descriptor) = match spec.rfind(':') {
        Some(at) => {
            let descriptor = spec.get(at + 1..)?;
            if !crate::valid_field_descriptor(descriptor) {
                return None;
            }
            (spec.get(..at)?, Some(descriptor.to_owned()))
        }
        None => (spec, None),
    };
    let (owner, name): (Option<String>, &str) = if let Some((owner, name)) = head.split_once('#') {
        ((!owner.is_empty()).then(|| owner.to_owned()), name)
    } else if head.starts_with('L') && head.contains(';') {
        let (owner, name) = head.strip_prefix('L')?.split_once(';')?;
        if owner.is_empty() {
            return None;
        }
        (Some(owner.to_owned()), name)
    } else if let Some(at) = head.rfind('.') {
        let owner = head.get(..at)?;
        let name = head.get(at + 1..)?;
        if owner.is_empty() {
            return None;
        }
        (Some(owner.to_owned()), name)
    } else {
        (None, head)
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'))
    {
        return None;
    }
    let owner = owner.map(|owner| owner.replace('.', "/"));
    if owner.as_deref().is_some_and(|owner| {
        owner.is_empty()
            || owner.starts_with('/')
            || owner.ends_with('/')
            || !owner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '$'))
    }) {
        return None;
    }
    Some(FieldSite {
        access: FieldAccess::Any,
        owner,
        name: name.to_owned(),
        descriptor,
    })
}

/// Parse `@At("NEW")`'s internal class name.
fn parse_new_site(spec: &str) -> Option<NewSite> {
    let spec = spec.trim();
    let inner = spec
        .strip_prefix('L')
        .and_then(|spec| spec.strip_suffix(';'))
        .unwrap_or(spec);
    if inner.is_empty()
        || inner.contains(char::is_whitespace)
        || !inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '$'))
    {
        return None;
    }
    let inner = inner.replace('.', "/");
    (!inner.is_empty() && !inner.starts_with('/') && !inner.ends_with('/'))
        .then_some(NewSite { owner: inner })
}

/// The jump opcodes a `@At("JUMP")` request may bound.
fn is_bounded_jump_opcode(opcode: u8) -> bool {
    matches!(opcode, 0x99..=0xa7 | 0xc6 | 0xc7)
}

/// Parse one `@At(value = "INVOKE", target = ...)` selector, accepting the
/// forms Mixin configs write: an internal owner, a binary owner with dots, or
/// an owner-less `name`, any with an optional method descriptor.  A selector
/// without a descriptor matches every overload of the name.
fn parse_invoke_site(spec: &str) -> Option<MethodRef> {
    let spec = spec.trim();
    if spec.is_empty() || spec.contains(char::is_whitespace) {
        return None;
    }
    let (head, descriptor) = match spec.find('(') {
        Some(at) => {
            let descriptor = spec.get(at..)?;
            if !crate::valid_method_descriptor(descriptor) {
                return None;
            }
            (spec.get(..at)?, descriptor)
        }
        None => (spec, ""),
    };
    let (owner, name) = if head.contains(';') {
        let (owner, name) = head.strip_prefix('L')?.split_once(';')?;
        (owner, name)
    } else if let Some((owner, name)) = head.split_once('#') {
        (owner, name)
    } else {
        let at = head.rfind('.')?;
        (head.get(..at)?, head.get(at + 1..)?)
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '<' | '>'))
        || !owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '$'))
    {
        return None;
    }
    let owner = owner.replace('.', "/");
    if owner.is_empty() || owner.starts_with('/') || owner.ends_with('/') {
        return None;
    }
    Some(MethodRef {
        owner,
        name: name.to_owned(),
        descriptor: descriptor.to_owned(),
    })
}

/// Parse one `@Slice` boundary label (`HEAD`, `RETURN`, `TAIL`, or
/// `INVOKE:<call site>`) exactly as discovery does.
fn anchor_slice_selector(label: &str) -> Result<SiteSelector, String> {
    match label {
        "HEAD" => Ok(SiteSelector::Head),
        "RETURN" => Ok(SiteSelector::Return),
        "TAIL" => Ok(SiteSelector::Tail),
        _ => {
            let spec = label.strip_prefix("INVOKE:").ok_or_else(|| {
                format!(
                    "@Slice anchor {label:?} is not a bounded boundary; native slices support HEAD, RETURN, TAIL and INVOKE"
                )
            })?;
            let site = parse_invoke_site(spec).ok_or_else(|| {
                format!("@Slice INVOKE boundary {spec:?} is not one of the bounded call-site forms")
            })?;
            Ok(SiteSelector::Invoke {
                owner: site.owner,
                name: site.name,
                descriptor: site.descriptor,
            })
        }
    }
}

/// Read one anchor plan's raw `@Slice` as the bounded half-open instruction
/// range [`SliceBounds`] names.
fn anchor_slice_bounds(plan: &MixinAnchorPlan) -> Result<Option<SliceBounds>, String> {
    match (plan.slice_from.as_deref(), plan.slice_to.as_deref()) {
        (None, None) => Ok(None),
        (Some(from), Some(to)) => Ok(Some(SliceBounds {
            from: anchor_slice_selector(from)?,
            to: anchor_slice_selector(to)?,
        })),
        _ => Err(
            "@Slice needs both a `from` and a `to` anchor; one bound alone does not name a range"
                .to_owned(),
        ),
    }
}

#[derive(Debug, Clone, Copy)]
struct Bytes<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| "class file offset overflow".to_owned())?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| format!("class file is truncated at offset {}", self.at))?;
        self.at = end;
        Ok(slice)
    }

    fn skip(&mut self, n: usize) -> Result<(), String> {
        self.bytes(n).map(|_| ())
    }

    fn u1(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    fn u2(&mut self) -> Result<u16, String> {
        let bytes = self.bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u4(&mut self) -> Result<u32, String> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

/// One entry of a `Code` attribute's exception table.
#[derive(Debug, Clone, Copy)]
struct ExceptionHandler {
    start_pc: u16,
    end_pc: u16,
    handler_pc: u16,
    catch_type: u16,
}

/// One `Code` attribute, split at the bytes a rewrite touches.
struct CodeLayout<'a> {
    max_stack: u16,
    max_locals: u16,
    code: &'a [u8],
    exception_table: Vec<ExceptionHandler>,
    /// The Code attribute's own attributes, in file order.  Tables that name a
    /// bytecode offset are parsed so a rewrite can shift them; every other
    /// attribute keeps its bytes.
    attributes: Vec<CodeAttribute<'a>>,
}

/// One attribute of a `Code` attribute.
enum CodeAttribute<'a> {
    /// An attribute the weaver does not read: copied verbatim.
    Raw { name_index: u16, body: &'a [u8] },
    /// `LineNumberTable`.
    LineNumbers {
        name_index: u16,
        lines: Vec<LineNumber>,
    },
    /// `LocalVariableTable` or `LocalVariableTypeTable`, whose rows share a
    /// layout.
    LocalVariables {
        name_index: u16,
        entries: Vec<LocalVariable>,
    },
    /// `StackMapTable`, the verifier's frames.
    StackMap {
        name_index: u16,
        frames: Vec<StackMapFrame>,
    },
}

impl CodeAttribute<'_> {
    fn name_index(&self) -> u16 {
        match self {
            Self::Raw { name_index, .. }
            | Self::LineNumbers { name_index, .. }
            | Self::LocalVariables { name_index, .. }
            | Self::StackMap { name_index, .. } => *name_index,
        }
    }
}

struct LineNumber {
    start_pc: u16,
    line_number: u16,
}

struct LocalVariable {
    start_pc: u16,
    length: u16,
    name_index: u16,
    descriptor_index: u16,
    index: u16,
}

/// One `StackMapTable` frame with its absolute bytecode offset.
struct StackMapFrame {
    /// The parsed `frame_type`, kept so an unshifted table re-encodes
    /// byte-exactly; a shifted table widens the type only when the new
    /// `offset_delta` no longer fits it.
    frame_type: u8,
    offset: usize,
    locals: Vec<VerificationType>,
    stack: Vec<VerificationType>,
}

/// One `verification_type_info`.
#[derive(Debug, Clone, Copy)]
enum VerificationType {
    /// Tags 0..=6 (`Top`, `Integer`, ... `Null`).
    Simple(u8),
    Object(u16),
    /// A `new` offset, which a rewrite shifts like a branch target.
    Uninitialized(u16),
}

struct AttributeLayout<'a> {
    name_index: u16,
    body: &'a [u8],
    code: Option<CodeLayout<'a>>,
}

struct MethodLayout<'a> {
    access: u16,
    name_index: u16,
    descriptor_index: u16,
    attributes: Vec<AttributeLayout<'a>>,
}

/// One actual constant-pool entry, in file order.
///
/// The writer reconstructs the pool from these entries so a constant-pool
/// relocation can rewrite every index-bearing tag, `CONSTANT_Dynamic` and
/// `CONSTANT_InvokeDynamic` included.  The payload is kept verbatim so an
/// untouched entry still re-encodes byte-for-byte.
struct PoolEntry<'a> {
    /// The index this entry occupies.  A `long`/`double` leaves the next slot
    /// empty, which is why this is not the vector position.
    index: u16,
    tag: u8,
    /// The bytes after the tag.
    payload: &'a [u8],
}

/// One `BootstrapMethods` row: the bootstrap method handle and its static
/// arguments, both constant-pool indices.
struct BootstrapMethod {
    method_ref: u16,
    arguments: Vec<u16>,
}

/// One class attribute.  `BootstrapMethods` is parsed because a relocated pool
/// has to keep every `CONSTANT_Dynamic`/`CONSTANT_InvokeDynamic` row pointing at
/// the same constants; every other attribute keeps its bytes.
enum ClassAttribute<'a> {
    Raw {
        name_index: u16,
        body: &'a [u8],
    },
    BootstrapMethods {
        name_index: u16,
        methods: Vec<BootstrapMethod>,
    },
}

impl ClassAttribute<'_> {
    fn name_index(&self) -> u16 {
        match self {
            Self::Raw { name_index, .. } | Self::BootstrapMethods { name_index, .. } => *name_index,
        }
    }
}

/// The class file as ordered pieces, so the writer can copy everything it does
/// not mean to change.
struct Layout<'a> {
    /// `magic`, `minor_version`, `major_version`.
    version: &'a [u8],
    pool_count: u16,
    /// Every actual pool entry, in file order.
    pool_entries: Vec<PoolEntry<'a>>,
    /// `Option<&str>` per constant pool index; `None` for non-`Utf8` entries.
    pool_utf8: Vec<Option<&'a str>>,
    /// Constant-pool tag per index (0 for the reserved slot and the second slot
    /// of a `long`/`double`), used to validate `CONSTANT_Dynamic` references.
    pool_tags: Vec<u8>,
    /// `access_flags`, `this_class`, `super_class` and the interfaces table.
    after_pool: &'a [u8],
    /// The whole field table, count included.
    fields: &'a [u8],
    methods: Vec<MethodLayout<'a>>,
    /// The class attribute table, parsed far enough to relocate
    /// `BootstrapMethods`.
    class_attributes: Vec<ClassAttribute<'a>>,
    /// Bytes after the class attributes.  Kept so `write` is byte-exact even
    /// for a class file that carries trailing data.
    tail: &'a [u8],
}

/// An old-to-new constant-pool index map.
///
/// Appending constants never moves an existing entry, so the writer normally
/// uses the identity map.  A caller that compacts the pool can pass any
/// one-to-one map; [`Layout::write_pool`] and [`Layout::write_class_attributes`]
/// apply it to every index the entries and `BootstrapMethods` rows name,
/// `CONSTANT_Dynamic` included.
struct PoolMap(Vec<u16>);

impl PoolMap {
    fn identity(count: u16) -> Self {
        Self((0..count).collect())
    }

    fn get(&self, index: u16) -> Result<u16, String> {
        self.0.get(index as usize).copied().ok_or_else(|| {
            format!(
                "constant pool index {index} is outside the {}-entry pool being written",
                self.0.len()
            )
        })
    }
}

impl<'a> Layout<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, String> {
        let mut r = Bytes::new(bytes);
        if r.u4()? != 0xCAFE_BABE {
            return Err("class file has bad magic".to_owned());
        }
        r.skip(4)?; // minor_version, major_version
        let version = &bytes[..8];

        let pool_count = r.u2()?;
        let (pool_utf8, pool_entries) = parse_pool(&mut r, pool_count)?;
        let mut pool_tags = vec![0u8; pool_count as usize];
        for entry in &pool_entries {
            pool_tags[entry.index as usize] = entry.tag;
        }

        let after_pool_start = r.at;
        let _access = r.u2()?;
        let _this_class = r.u2()?;
        let _super_class = r.u2()?;
        let interfaces = r.u2()? as usize;
        r.skip(
            interfaces
                .checked_mul(2)
                .ok_or("interfaces count overflow")?,
        )?;
        let after_pool = &bytes[after_pool_start..r.at];

        let fields_start = r.at;
        let field_count = r.u2()? as usize;
        for _ in 0..field_count {
            r.skip(6)?; // access_flags, name_index, descriptor_index
            skip_attributes(&mut r)?;
        }
        let fields = &bytes[fields_start..r.at];

        let method_count = r.u2()? as usize;
        let mut methods = Vec::with_capacity(method_count);
        for _ in 0..method_count {
            let access = r.u2()?;
            let name_index = r.u2()?;
            let descriptor_index = r.u2()?;
            let attribute_count = r.u2()? as usize;
            let mut attributes = Vec::with_capacity(attribute_count);
            for _ in 0..attribute_count {
                let name_index = r.u2()?;
                let length = r.u4()? as usize;
                let body = r.bytes(length)?;
                let code = if pool_utf8.get(name_index as usize).copied().flatten() == Some("Code")
                {
                    Some(parse_code(body, &pool_utf8)?)
                } else {
                    None
                };
                attributes.push(AttributeLayout {
                    name_index,
                    body,
                    code,
                });
            }
            methods.push(MethodLayout {
                access,
                name_index,
                descriptor_index,
                attributes,
            });
        }

        let class_attribute_count = r.u2()? as usize;
        let mut class_attributes = Vec::with_capacity(class_attribute_count);
        for _ in 0..class_attribute_count {
            let name_index = r.u2()?;
            let length = r.u4()? as usize;
            let body = r.bytes(length)?;
            class_attributes.push(
                if pool_utf8.get(name_index as usize).copied().flatten() == Some("BootstrapMethods")
                {
                    ClassAttribute::BootstrapMethods {
                        name_index,
                        methods: parse_bootstrap_methods(body)?,
                    }
                } else {
                    ClassAttribute::Raw { name_index, body }
                },
            );
        }
        let tail = &bytes[r.at..];

        Ok(Self {
            version,
            pool_count,
            pool_entries,
            pool_utf8,
            pool_tags,
            after_pool,
            fields,
            methods,
            class_attributes,
            tail,
        })
    }

    fn utf8(&self, index: u16) -> Option<&'a str> {
        self.pool_utf8.get(index as usize).copied().flatten()
    }

    fn method_name(&self, method: &MethodLayout<'a>) -> Option<&'a str> {
        self.utf8(method.name_index)
    }

    fn method_descriptor(&self, method: &MethodLayout<'a>) -> Option<&'a str> {
        self.utf8(method.descriptor_index)
    }

    /// The one `BootstrapMethods` row list, if the class declares the attribute.
    fn bootstrap_methods(&self) -> Option<&[BootstrapMethod]> {
        self.class_attributes
            .iter()
            .find_map(|attribute| match attribute {
                ClassAttribute::BootstrapMethods { methods, .. } => Some(methods.as_slice()),
                ClassAttribute::Raw { .. } => None,
            })
    }

    fn pool_entry(&self, index: u16) -> Option<&PoolEntry<'a>> {
        self.pool_entries.iter().find(|entry| entry.index == index)
    }

    fn pool_tag(&self, index: u16) -> Option<u8> {
        self.pool_tags
            .get(index as usize)
            .copied()
            .filter(|tag| *tag != 0)
    }

    /// The name and descriptor one `CONSTANT_NameAndType` names.
    fn name_type(&self, index: u16) -> Result<(&'a str, &'a str), String> {
        let entry = self
            .pool_entry(index)
            .filter(|entry| entry.tag == 12)
            .ok_or_else(|| format!("constant pool index {index} is not a CONSTANT_NameAndType"))?;
        let name_index = payload_u2(entry.payload, 0)?;
        let descriptor_index = payload_u2(entry.payload, 2)?;
        let name = self
            .utf8(name_index)
            .ok_or_else(|| format!("constant pool index {name_index} is not a Utf8 name"))?;
        let descriptor = self.utf8(descriptor_index).ok_or_else(|| {
            format!("constant pool index {descriptor_index} is not a Utf8 descriptor")
        })?;
        Ok((name, descriptor))
    }

    /// Check every `CONSTANT_Dynamic` entry and its bootstrap chain.
    ///
    /// Appending constants after the original entries cannot move an index, so
    /// the writer does not have to renumber anything to preserve a condy pool.
    /// It does have to be sure the class it writes is coherent, which is what
    /// this validation establishes: the name/type is a field descriptor, the
    /// bootstrap index names a `BootstrapMethods` row, that row's handle is a
    /// legal `CONSTANT_MethodHandle`, and every static argument is a loadable
    /// constant (recursively, for another `CONSTANT_Dynamic`).
    ///
    /// A malformed entry that names the reserved index 0 keeps the historical
    /// blanket rejection: that is not a condy shape the writer can relocate,
    /// and a class built by splicing an empty tag is outside the subset.
    fn validate_constant_pool(&self) -> Result<(), String> {
        if !self.pool_tags.contains(&17) {
            return Ok(());
        }
        let bootstrap_attributes = self
            .class_attributes
            .iter()
            .filter(|attribute| matches!(attribute, ClassAttribute::BootstrapMethods { .. }))
            .count();
        if bootstrap_attributes > 1 {
            return Err("class declares more than one BootstrapMethods attribute".to_owned());
        }
        let mut visiting = BTreeSet::new();
        for entry in &self.pool_entries {
            if entry.tag == 17 {
                self.validate_dynamic(entry.index, &mut visiting)?;
            }
        }
        Ok(())
    }

    fn validate_dynamic(&self, index: u16, visiting: &mut BTreeSet<u16>) -> Result<(), String> {
        if !visiting.insert(index) {
            return Err(format!(
                "CONSTANT_Dynamic {index} is part of a bootstrap cycle"
            ));
        }
        let entry = self
            .pool_entry(index)
            .filter(|entry| entry.tag == 17)
            .ok_or_else(|| format!("constant pool index {index} is not a CONSTANT_Dynamic"))?;
        let bootstrap = payload_u2(entry.payload, 0)?;
        let nt = payload_u2(entry.payload, 2)?;
        if nt == 0 {
            // The reserved index 0 is not a shape this writer can relocate.
            return Err(
                "class constant pool uses CONSTANT_Dynamic (condy), which the bounded weaver does not rewrite"
                    .to_owned(),
            );
        }
        let (name, descriptor) = self.name_type(nt)?;
        if name.is_empty() || !crate::valid_field_descriptor(descriptor) {
            return Err(format!(
                "CONSTANT_Dynamic {index} names {name:?}{descriptor}, which is not a field descriptor"
            ));
        }
        let methods = self.bootstrap_methods().ok_or_else(|| {
            format!("CONSTANT_Dynamic {index} has no BootstrapMethods attribute to name")
        })?;
        let method = methods.get(bootstrap as usize).ok_or_else(|| {
            format!(
                "CONSTANT_Dynamic {index} names bootstrap method {bootstrap}, but the class declares {}",
                methods.len()
            )
        })?;
        self.validate_bootstrap_handle(method.method_ref, index)?;
        for (position, argument) in method.arguments.iter().enumerate() {
            match self.pool_tag(*argument) {
                Some(17) => self.validate_dynamic(*argument, visiting)?,
                Some(3 | 4 | 5 | 6 | 7 | 8 | 15 | 16) => {}
                Some(18) => {
                    return Err(format!(
                        "CONSTANT_Dynamic {index}'s static argument {position} is a CONSTANT_InvokeDynamic, which is not a loadable constant"
                    ));
                }
                Some(tag) => {
                    return Err(format!(
                        "CONSTANT_Dynamic {index}'s static argument {position} (index {argument}) has tag {tag}, which is not a loadable constant"
                    ));
                }
                None => {
                    return Err(format!(
                        "CONSTANT_Dynamic {index}'s static argument {position} names constant pool index {argument}, which the class does not declare"
                    ));
                }
            }
        }
        visiting.remove(&index);
        Ok(())
    }

    fn validate_bootstrap_handle(&self, handle_index: u16, dynamic: u16) -> Result<(), String> {
        let entry = self
            .pool_entry(handle_index)
            .filter(|entry| entry.tag == 15)
            .ok_or_else(|| {
                format!(
                    "CONSTANT_Dynamic {dynamic} names bootstrap method {handle_index}, which is not a CONSTANT_MethodHandle"
                )
            })?;
        let kind = entry
            .payload
            .first()
            .copied()
            .ok_or_else(|| "a CONSTANT_MethodHandle has no reference kind".to_owned())?;
        let reference = payload_u2(entry.payload, 1)?;
        let reference_tag = self.pool_tag(reference).unwrap_or(0);
        let legal = match kind {
            1..=4 => reference_tag == 9,
            5 | 8 => reference_tag == 10,
            6 | 7 => matches!(reference_tag, 10 | 11),
            9 => reference_tag == 11,
            _ => false,
        };
        if !legal {
            return Err(format!(
                "CONSTANT_Dynamic {dynamic}'s bootstrap method handle has kind {kind} and references constant-pool tag {reference_tag}, which is not a legal handle target"
            ));
        }
        Ok(())
    }

    /// Re-encode the constant pool through an index map.
    ///
    /// The identity map reproduces the input bytes exactly; a compaction map
    /// rewrites every index-bearing entry, including `CONSTANT_Dynamic`'s
    /// `bootstrap_method_attr_index` and `name_and_type_index`.
    fn write_pool(&self, out: &mut Vec<u8>, map: &PoolMap) -> Result<(), String> {
        // Entries are written in their *new* index order, so a compaction that
        // moves an entry also moves its bytes; every index a payload names is
        // rewritten through the same map.
        let mut ordered = self
            .pool_entries
            .iter()
            .map(|entry| Ok((map.get(entry.index)?, entry)))
            .collect::<Result<Vec<_>, String>>()?;
        ordered.sort_by_key(|(index, _)| *index);
        let mut expected = 1u16;
        for (index, entry) in &ordered {
            if *index != expected {
                return Err(format!(
                    "constant-pool relocation is not contiguous at index {index} (expected {expected})"
                ));
            }
            expected = expected
                .checked_add(if matches!(entry.tag, 5 | 6) { 2 } else { 1 })
                .ok_or("constant pool index overflow")?;
        }
        for (_, entry) in ordered {
            out.push(entry.tag);
            match entry.tag {
                1 | 3 | 4 | 5 | 6 => out.extend_from_slice(entry.payload),
                7 | 8 | 16 | 19 | 20 => push_u2(out, map.get(payload_u2(entry.payload, 0)?)?),
                9 | 10 | 11 | 12 | 17 | 18 => {
                    push_u2(out, map.get(payload_u2(entry.payload, 0)?)?);
                    push_u2(out, map.get(payload_u2(entry.payload, 2)?)?);
                }
                15 => {
                    let kind = entry.payload.first().copied().ok_or_else(|| {
                        "a CONSTANT_MethodHandle has no reference kind".to_owned()
                    })?;
                    out.push(kind);
                    push_u2(out, map.get(payload_u2(entry.payload, 1)?)?);
                }
                other => {
                    return Err(format!(
                        "constant pool tag {other} is not supported by the class writer"
                    ));
                }
            }
        }
        Ok(())
    }

    fn write_class_attributes(&self, out: &mut Vec<u8>, map: &PoolMap) -> Result<(), String> {
        let count = u16::try_from(self.class_attributes.len())
            .map_err(|_| "class file has too many attributes".to_owned())?;
        out.extend_from_slice(&count.to_be_bytes());
        for attribute in &self.class_attributes {
            out.extend_from_slice(&attribute.name_index().to_be_bytes());
            let body = match attribute {
                ClassAttribute::Raw { body, .. } => body.to_vec(),
                ClassAttribute::BootstrapMethods { methods, .. } => {
                    encode_bootstrap_methods(methods, map)?
                }
            };
            let length = u32::try_from(body.len())
                .map_err(|_| "attribute is longer than 4 GiB".to_owned())?;
            out.extend_from_slice(&length.to_be_bytes());
            out.extend_from_slice(&body);
        }
        Ok(())
    }
}

fn skip_attributes(r: &mut Bytes<'_>) -> Result<(), String> {
    let count = r.u2()? as usize;
    for _ in 0..count {
        r.skip(2)?; // attribute_name_index
        let length = r.u4()? as usize;
        r.skip(length)?;
    }
    Ok(())
}

fn parse_pool<'a>(
    r: &mut Bytes<'a>,
    count: u16,
) -> Result<(Vec<Option<&'a str>>, Vec<PoolEntry<'a>>), String> {
    if count == 0 {
        return Err("class file constant pool count is zero".to_owned());
    }
    let mut utf8 = vec![None; count as usize];
    let mut entries = Vec::new();
    let mut index = 1usize;
    while index < count as usize {
        let offset = r.at;
        let entry_index = u16::try_from(index).map_err(|_| "constant pool index overflow")?;
        let tag = r.u1()?;
        let payload_start = r.at;
        match tag {
            1 => {
                let length = r.u2()? as usize;
                if let Ok(value) = std::str::from_utf8(r.bytes(length)?) {
                    utf8[index] = Some(value);
                }
            }
            3 | 4 => r.skip(4)?,
            5 | 6 => {
                r.skip(8)?;
                // Long and double take two constant pool slots.
                index += 1;
            }
            7 | 8 | 16 | 19 | 20 => r.skip(2)?,
            9 | 10 | 11 | 12 | 17 | 18 => r.skip(4)?,
            15 => r.skip(3)?,
            other => {
                return Err(format!(
                    "constant pool tag {other} at offset {offset} is not supported by the class writer"
                ));
            }
        }
        entries.push(PoolEntry {
            index: entry_index,
            tag,
            payload: &r.bytes[payload_start..r.at],
        });
        index += 1;
    }
    Ok((utf8, entries))
}

fn parse_bootstrap_methods(body: &[u8]) -> Result<Vec<BootstrapMethod>, String> {
    let mut r = Bytes::new(body);
    let count = r.u2()? as usize;
    let mut methods = Vec::with_capacity(count);
    for _ in 0..count {
        let method_ref = r.u2()?;
        let argument_count = r.u2()? as usize;
        let mut arguments = Vec::with_capacity(argument_count);
        for _ in 0..argument_count {
            arguments.push(r.u2()?);
        }
        methods.push(BootstrapMethod {
            method_ref,
            arguments,
        });
    }
    Ok(methods)
}

fn parse_code<'a>(body: &'a [u8], pool: &[Option<&'a str>]) -> Result<CodeLayout<'a>, String> {
    let mut r = Bytes::new(body);
    let max_stack = r.u2()?;
    let max_locals = r.u2()?;
    let code_length = r.u4()? as usize;
    let code = r.bytes(code_length)?;
    let handlers = r.u2()? as usize;
    let mut exception_table = Vec::with_capacity(handlers);
    for _ in 0..handlers {
        exception_table.push(ExceptionHandler {
            start_pc: r.u2()?,
            end_pc: r.u2()?,
            handler_pc: r.u2()?,
            catch_type: r.u2()?,
        });
    }
    let attribute_count = r.u2()? as usize;
    let mut attributes = Vec::with_capacity(attribute_count);
    for _ in 0..attribute_count {
        let name_index = r.u2()?;
        let length = r.u4()? as usize;
        let attribute = r.bytes(length)?;
        attributes.push(parse_code_attribute(pool, name_index, attribute)?);
    }
    Ok(CodeLayout {
        max_stack,
        max_locals,
        code,
        exception_table,
        attributes,
    })
}

fn parse_code_attribute<'a>(
    pool: &[Option<&'a str>],
    name_index: u16,
    body: &'a [u8],
) -> Result<CodeAttribute<'a>, String> {
    let name = pool.get(name_index as usize).copied().flatten();
    Ok(match name {
        Some("LineNumberTable") => CodeAttribute::LineNumbers {
            name_index,
            lines: parse_line_numbers(body)?,
        },
        Some("LocalVariableTable" | "LocalVariableTypeTable") => CodeAttribute::LocalVariables {
            name_index,
            entries: parse_local_variables(body)?,
        },
        Some("StackMapTable") => CodeAttribute::StackMap {
            name_index,
            frames: parse_stack_map(body)?,
        },
        _ => CodeAttribute::Raw { name_index, body },
    })
}

fn parse_line_numbers(body: &[u8]) -> Result<Vec<LineNumber>, String> {
    let mut r = Bytes::new(body);
    let count = r.u2()? as usize;
    let mut lines = Vec::with_capacity(count);
    for _ in 0..count {
        lines.push(LineNumber {
            start_pc: r.u2()?,
            line_number: r.u2()?,
        });
    }
    Ok(lines)
}

fn parse_local_variables(body: &[u8]) -> Result<Vec<LocalVariable>, String> {
    let mut r = Bytes::new(body);
    let count = r.u2()? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push(LocalVariable {
            start_pc: r.u2()?,
            length: r.u2()?,
            name_index: r.u2()?,
            descriptor_index: r.u2()?,
            index: r.u2()?,
        });
    }
    Ok(entries)
}

fn parse_verification(r: &mut Bytes<'_>) -> Result<VerificationType, String> {
    let tag = r.u1()?;
    Ok(match tag {
        0..=6 => VerificationType::Simple(tag),
        7 => VerificationType::Object(r.u2()?),
        8 => VerificationType::Uninitialized(r.u2()?),
        other => return Err(format!("stack-map verification type {other} is reserved")),
    })
}

fn parse_stack_map(body: &[u8]) -> Result<Vec<StackMapFrame>, String> {
    let mut r = Bytes::new(body);
    let count = r.u2()? as usize;
    let mut frames: Vec<StackMapFrame> = Vec::with_capacity(count);
    for _ in 0..count {
        let frame_type = r.u1()?;
        let (delta, locals, stack) = match frame_type {
            0..=63 => (frame_type as usize, Vec::new(), Vec::new()),
            64..=127 => (
                frame_type as usize - 64,
                Vec::new(),
                vec![parse_verification(&mut r)?],
            ),
            128..=246 => {
                return Err(format!(
                    "StackMapTable frame type {frame_type} is reserved; the bounded weaver cannot re-encode it"
                ));
            }
            247 => (
                r.u2()? as usize,
                Vec::new(),
                vec![parse_verification(&mut r)?],
            ),
            248..=251 => (r.u2()? as usize, Vec::new(), Vec::new()),
            252..=254 => {
                let delta = r.u2()? as usize;
                let locals = parse_verifications(&mut r, frame_type as usize - 251)?;
                (delta, locals, Vec::new())
            }
            _ => {
                let delta = r.u2()? as usize;
                let local_count = r.u2()? as usize;
                let locals = parse_verifications(&mut r, local_count)?;
                let stack_count = r.u2()? as usize;
                let stack = parse_verifications(&mut r, stack_count)?;
                (delta, locals, stack)
            }
        };
        let offset = match frames.last() {
            Some(previous) => previous.offset + delta + 1,
            None => delta,
        };
        frames.push(StackMapFrame {
            frame_type,
            offset,
            locals,
            stack,
        });
    }
    Ok(frames)
}

fn parse_verifications(r: &mut Bytes<'_>, count: usize) -> Result<Vec<VerificationType>, String> {
    let mut types = Vec::with_capacity(count);
    for _ in 0..count {
        types.push(parse_verification(r)?);
    }
    Ok(types)
}

/// One appended constant: only the shapes the weaver interns.
fn encode_constant(out: &mut Vec<u8>, constant: &Cp) -> Result<(), String> {
    match constant {
        Cp::Utf8(value) => {
            if !value.is_ascii() {
                return Err(format!(
                    "constant pool string {value:?} is not ASCII; the bounded writer cannot encode modified UTF-8"
                ));
            }
            let length = u16::try_from(value.len())
                .map_err(|_| "constant pool string is longer than 65535 bytes".to_owned())?;
            out.push(1);
            out.extend_from_slice(&length.to_be_bytes());
            out.extend_from_slice(value.as_bytes());
        }
        Cp::Class(name) => {
            out.push(7);
            out.extend_from_slice(&name.to_be_bytes());
        }
        Cp::NameType { name, descriptor } => {
            out.push(12);
            out.extend_from_slice(&name.to_be_bytes());
            out.extend_from_slice(&descriptor.to_be_bytes());
        }
        Cp::FieldRef { class, nt } => {
            out.push(9);
            out.extend_from_slice(&class.to_be_bytes());
            out.extend_from_slice(&nt.to_be_bytes());
        }
        Cp::MethodRef { class, nt } => {
            out.push(10);
            out.extend_from_slice(&class.to_be_bytes());
            out.extend_from_slice(&nt.to_be_bytes());
        }
        Cp::InterfaceMethodRef { class, nt } => {
            out.push(11);
            out.extend_from_slice(&class.to_be_bytes());
            out.extend_from_slice(&nt.to_be_bytes());
        }
        other => {
            return Err(format!(
                "the class writer can only append Utf8/Class/NameAndType/FieldRef/MethodRef constants, not {other:?}"
            ));
        }
    }
    Ok(())
}

/// Interns the constants a callback reference needs, reusing the class's own
/// pool when it already holds the same entry.
struct PoolBuilder<'a> {
    pool: &'a [Cp],
    added: Vec<Cp>,
}

impl<'a> PoolBuilder<'a> {
    fn new(pool: &'a [Cp]) -> Self {
        Self {
            pool,
            added: Vec::new(),
        }
    }

    fn iter(&self) -> impl Iterator<Item = (usize, &Cp)> {
        self.pool.iter().enumerate().chain(
            self.added
                .iter()
                .enumerate()
                .map(|(i, cp)| (i + self.pool.len(), cp)),
        )
    }

    fn position(&self, mut matches: impl FnMut(&Cp) -> bool) -> Option<u16> {
        self.iter()
            .find(|(_, cp)| matches(cp))
            .and_then(|(index, _)| u16::try_from(index).ok())
    }

    fn push(&mut self, constant: Cp) -> Result<u16, String> {
        let index = self.pool.len() + self.added.len();
        let index =
            u16::try_from(index).map_err(|_| "constant pool has no free entries".to_owned())?;
        self.added.push(constant);
        Ok(index)
    }

    fn utf8(&mut self, value: &str) -> Result<u16, String> {
        if let Some(index) = self.position(|cp| matches!(cp, Cp::Utf8(v) if v == value)) {
            return Ok(index);
        }
        self.push(Cp::Utf8(value.to_owned()))
    }

    fn class(&mut self, name: &str) -> Result<u16, String> {
        let name_index = self.utf8(name)?;
        if let Some(index) = self.position(|cp| matches!(cp, Cp::Class(n) if *n == name_index)) {
            return Ok(index);
        }
        self.push(Cp::Class(name_index))
    }

    fn name_type(&mut self, name: &str, descriptor: &str) -> Result<u16, String> {
        let name_index = self.utf8(name)?;
        let descriptor_index = self.utf8(descriptor)?;
        if let Some(index) = self.position(|cp| {
            matches!(
                cp,
                Cp::NameType { name, descriptor }
                    if *name == name_index && *descriptor == descriptor_index
            )
        }) {
            return Ok(index);
        }
        self.push(Cp::NameType {
            name: name_index,
            descriptor: descriptor_index,
        })
    }

    fn method_ref(&mut self, method: &MethodRef) -> Result<u16, String> {
        let class = self.class(&method.owner)?;
        let nt = self.name_type(&method.name, &method.descriptor)?;
        if let Some(index) = self.position(|cp| match cp {
            Cp::MethodRef { class: c, nt: n } | Cp::InterfaceMethodRef { class: c, nt: n } => {
                *c == class && *n == nt
            }
            _ => false,
        }) {
            return Ok(index);
        }
        self.push(Cp::MethodRef { class, nt })
    }
}

fn push_u2(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Read a `u2` from a constant-pool entry payload.
fn payload_u2(payload: &[u8], at: usize) -> Result<u16, String> {
    let bytes = payload
        .get(at..at + 2)
        .ok_or_else(|| format!("constant pool entry is truncated at payload offset {at}"))?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Re-encode a `BootstrapMethods` table through an index map.
fn encode_bootstrap_methods(methods: &[BootstrapMethod], map: &PoolMap) -> Result<Vec<u8>, String> {
    let count = u16::try_from(methods.len())
        .map_err(|_| "BootstrapMethods has too many entries".to_owned())?;
    let mut out = Vec::new();
    push_u2(&mut out, count);
    for method in methods {
        push_u2(&mut out, map.get(method.method_ref)?);
        let arguments = u16::try_from(method.arguments.len())
            .map_err(|_| "a BootstrapMethods row has too many arguments".to_owned())?;
        push_u2(&mut out, arguments);
        for argument in &method.arguments {
            push_u2(&mut out, map.get(*argument)?);
        }
    }
    Ok(out)
}

fn push_u4(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Append one constant to a pool under construction and return its index.
///
/// Like a parsed pool, index 0 is the reserved `Cp::Empty` slot, so the count a
/// caller writes is `pool.len()` and the entries are `pool[1..]`.
fn pool_push(pool: &mut Vec<Cp>, constant: Cp) -> u16 {
    pool.push(constant);
    // A synthetic class stays far below the 65535-entry constant pool limit, so
    // the index of the entry just pushed always fits.
    (pool.len() - 1) as u16
}

fn pool_utf8(pool: &mut Vec<Cp>, value: &str) -> u16 {
    pool_push(pool, Cp::Utf8(value.to_owned()))
}

fn pool_class(pool: &mut Vec<Cp>, name: &str) -> u16 {
    let name = pool_utf8(pool, name);
    pool_push(pool, Cp::Class(name))
}

fn pool_name_type(pool: &mut Vec<Cp>, name: u16, descriptor: u16) -> u16 {
    pool_push(pool, Cp::NameType { name, descriptor })
}

fn pool_field_ref(pool: &mut Vec<Cp>, owner: &str, name: u16, descriptor: u16) -> u16 {
    let class = pool_class(pool, owner);
    let nt = pool_name_type(pool, name, descriptor);
    pool_push(pool, Cp::FieldRef { class, nt })
}

fn pool_method_ref(pool: &mut Vec<Cp>, owner: &str, name: u16, descriptor: u16) -> u16 {
    let class = pool_class(pool, owner);
    let nt = pool_name_type(pool, name, descriptor);
    pool_push(pool, Cp::MethodRef { class, nt })
}

/// One method body of a synthetic class.
struct SyntheticCode<'a> {
    max_stack: u16,
    max_locals: u16,
    bytes: &'a [u8],
}

fn push_synthetic_field(out: &mut Vec<u8>, access: u16, name: u16, descriptor: u16) {
    push_u2(out, access);
    push_u2(out, name);
    push_u2(out, descriptor);
    push_u2(out, 0); // no field attributes
}

fn push_synthetic_method(
    out: &mut Vec<u8>,
    code_name: u16,
    access: u16,
    name: u16,
    descriptor: u16,
    body: SyntheticCode<'_>,
) {
    push_u2(out, access);
    push_u2(out, name);
    push_u2(out, descriptor);
    push_u2(out, 1); // one attribute: Code
    push_u2(out, code_name);
    let mut attribute = Vec::new();
    push_u2(&mut attribute, body.max_stack);
    push_u2(&mut attribute, body.max_locals);
    push_u4(&mut attribute, body.bytes.len() as u32);
    attribute.extend_from_slice(body.bytes);
    push_u2(&mut attribute, 0); // exception table
    push_u2(&mut attribute, 0); // Code attributes
    push_u4(out, attribute.len() as u32);
    out.extend_from_slice(&attribute);
}

/// The bounded JVM-side `CallbackInfoReturnable` a woven non-void
/// `RETURN`/`TAIL` insert constructs.
///
/// Mixin's own class lives in the Mixin jar, which a jar woven without Mixin on
/// the classpath does not have, so the weaver writes the smallest class that
/// answers what a return callback uses: `returnValue`/`cancelled` fields, the
/// `()V` and `(Ljava/lang/Object;)V` constructors, `setReturnValue`,
/// `getReturnValue`, `cancel` and `isCancelled`.  No member branches, so the
/// class needs no `StackMapTable`, and it parses with [`crate::ClassFile::parse`]
/// exactly like a compiled class.
///
/// The class stands on its own rather than extending Mixin's `CallbackInfo`:
/// the woven jar may not carry that class, and a callback that casts this object
/// to `CallbackInfo` is outside the bounded subset.
pub fn callback_info_returnable_class() -> Result<Vec<u8>, String> {
    const NAME: &str = CALLBACK_INFO_RETURNABLE_CLASS;
    let mut pool: Vec<Cp> = vec![Cp::Empty];
    let this_class = pool_class(&mut pool, NAME);
    let super_class = pool_class(&mut pool, "java/lang/Object");
    let code_name = pool_utf8(&mut pool, "Code");
    // Members are named and described once, so the pool has no duplicate Utf8.
    let name_return_value = pool_utf8(&mut pool, "returnValue");
    let name_cancelled = pool_utf8(&mut pool, "cancelled");
    let name_init = pool_utf8(&mut pool, "<init>");
    let name_set_return = pool_utf8(&mut pool, "setReturnValue");
    let name_get_return = pool_utf8(&mut pool, "getReturnValue");
    let name_cancel = pool_utf8(&mut pool, "cancel");
    let name_is_cancelled = pool_utf8(&mut pool, "isCancelled");
    let desc_object = pool_utf8(&mut pool, "Ljava/lang/Object;");
    let desc_boolean = pool_utf8(&mut pool, "Z");
    let desc_void = pool_utf8(&mut pool, "()V");
    let desc_value = pool_utf8(&mut pool, "(Ljava/lang/Object;)V");
    let desc_get_return = pool_utf8(&mut pool, "()Ljava/lang/Object;");
    let desc_is_cancelled = pool_utf8(&mut pool, "()Z");
    let return_value = pool_field_ref(&mut pool, NAME, name_return_value, desc_object);
    let cancelled = pool_field_ref(&mut pool, NAME, name_cancelled, desc_boolean);
    let object_init = pool_method_ref(&mut pool, "java/lang/Object", name_init, desc_void);

    // `<init>()V`: `aload_0; invokespecial Object.<init>; return`.
    let mut init_bytes = vec![0x2a, 0xb7];
    init_bytes.extend_from_slice(&object_init.to_be_bytes());
    init_bytes.push(0xb1);
    // `<init>(Object)V`: the same, then `putfield returnValue` from `aload_1`.
    let mut init_value_bytes = vec![0x2a, 0xb7];
    init_value_bytes.extend_from_slice(&object_init.to_be_bytes());
    init_value_bytes.extend_from_slice(&[0x2a, 0x2b, 0xb5]);
    init_value_bytes.extend_from_slice(&return_value.to_be_bytes());
    init_value_bytes.push(0xb1);
    // `setReturnValue(Object)V`: `aload_0; aload_1; putfield returnValue`.
    let mut set_return_bytes = vec![0x2a, 0x2b, 0xb5];
    set_return_bytes.extend_from_slice(&return_value.to_be_bytes());
    set_return_bytes.push(0xb1);
    // `getReturnValue()Object`: `aload_0; getfield returnValue; areturn`.
    let mut get_return_bytes = vec![0x2a, 0xb4];
    get_return_bytes.extend_from_slice(&return_value.to_be_bytes());
    get_return_bytes.push(0xb0);
    // `cancel()V`: `aload_0; iconst_1; putfield cancelled; return`.
    let mut cancel_bytes = vec![0x2a, 0x04, 0xb5];
    cancel_bytes.extend_from_slice(&cancelled.to_be_bytes());
    cancel_bytes.push(0xb1);
    // `isCancelled()Z`: `aload_0; getfield cancelled; ireturn`.
    let mut is_cancelled_bytes = vec![0x2a, 0xb4];
    is_cancelled_bytes.extend_from_slice(&cancelled.to_be_bytes());
    is_cancelled_bytes.push(0xac);

    let mut out = Vec::new();
    push_u4(&mut out, 0xCAFE_BABE);
    push_u2(&mut out, 0); // minor_version
    push_u2(&mut out, SYNTHETIC_CLASS_MAJOR);
    push_u2(&mut out, pool.len() as u16);
    // Index 0 is the reserved slot, so the entries are `pool[1..]`.
    for constant in &pool[1..] {
        encode_constant(&mut out, constant)?;
    }
    push_u2(&mut out, ACC_PUBLIC | ACC_SUPER);
    push_u2(&mut out, this_class);
    push_u2(&mut out, super_class);
    push_u2(&mut out, 0); // interfaces
    push_u2(&mut out, 2); // fields
    push_synthetic_field(&mut out, ACC_PRIVATE, name_return_value, desc_object);
    push_synthetic_field(&mut out, ACC_PRIVATE, name_cancelled, desc_boolean);
    push_u2(&mut out, 6); // methods
    push_synthetic_method(
        &mut out,
        code_name,
        ACC_PUBLIC,
        name_init,
        desc_void,
        SyntheticCode {
            max_stack: 1,
            max_locals: 1,
            bytes: &init_bytes,
        },
    );
    push_synthetic_method(
        &mut out,
        code_name,
        ACC_PUBLIC,
        name_init,
        desc_value,
        SyntheticCode {
            max_stack: 2,
            max_locals: 2,
            bytes: &init_value_bytes,
        },
    );
    push_synthetic_method(
        &mut out,
        code_name,
        ACC_PUBLIC,
        name_set_return,
        desc_value,
        SyntheticCode {
            max_stack: 2,
            max_locals: 2,
            bytes: &set_return_bytes,
        },
    );
    push_synthetic_method(
        &mut out,
        code_name,
        ACC_PUBLIC,
        name_get_return,
        desc_get_return,
        SyntheticCode {
            max_stack: 1,
            max_locals: 1,
            bytes: &get_return_bytes,
        },
    );
    push_synthetic_method(
        &mut out,
        code_name,
        ACC_PUBLIC,
        name_cancel,
        desc_void,
        SyntheticCode {
            max_stack: 2,
            max_locals: 1,
            bytes: &cancel_bytes,
        },
    );
    push_synthetic_method(
        &mut out,
        code_name,
        ACC_PUBLIC,
        name_is_cancelled,
        desc_is_cancelled,
        SyntheticCode {
            max_stack: 1,
            max_locals: 1,
            bytes: &is_cancelled_bytes,
        },
    );
    push_u2(&mut out, 0); // class attributes
    Ok(out)
}

/// The bounded JVM-side `CallbackInfo` a woven cancellable `void` `HEAD`
/// insert constructs.
///
/// Like [`callback_info_returnable_class`], this is the smallest class a callback
/// needs rather than Mixin's own: a `cancelled` field, the `()V` constructor,
/// `cancel` and `isCancelled`.  No member branches, so it needs no
/// `StackMapTable`.
pub fn callback_info_class() -> Result<Vec<u8>, String> {
    const NAME: &str = CALLBACK_INFO_CLASS;
    let mut pool: Vec<Cp> = vec![Cp::Empty];
    let this_class = pool_class(&mut pool, NAME);
    let super_class = pool_class(&mut pool, "java/lang/Object");
    let code_name = pool_utf8(&mut pool, "Code");
    let name_cancelled = pool_utf8(&mut pool, "cancelled");
    let name_init = pool_utf8(&mut pool, "<init>");
    let name_cancel = pool_utf8(&mut pool, "cancel");
    let name_is_cancelled = pool_utf8(&mut pool, "isCancelled");
    let desc_boolean = pool_utf8(&mut pool, "Z");
    let desc_void = pool_utf8(&mut pool, "()V");
    let desc_is_cancelled = pool_utf8(&mut pool, "()Z");
    let cancelled = pool_field_ref(&mut pool, NAME, name_cancelled, desc_boolean);
    let object_init = pool_method_ref(&mut pool, "java/lang/Object", name_init, desc_void);

    // `<init>()V`: `aload_0; invokespecial Object.<init>; return`.
    let mut init_bytes = vec![0x2a, 0xb7];
    init_bytes.extend_from_slice(&object_init.to_be_bytes());
    init_bytes.push(0xb1);
    // `cancel()V`: `aload_0; iconst_1; putfield cancelled; return`.
    let mut cancel_bytes = vec![0x2a, 0x04, 0xb5];
    cancel_bytes.extend_from_slice(&cancelled.to_be_bytes());
    cancel_bytes.push(0xb1);
    // `isCancelled()Z`: `aload_0; getfield cancelled; ireturn`.
    let mut is_cancelled_bytes = vec![0x2a, 0xb4];
    is_cancelled_bytes.extend_from_slice(&cancelled.to_be_bytes());
    is_cancelled_bytes.push(0xac);

    let mut out = Vec::new();
    push_u4(&mut out, 0xCAFE_BABE);
    push_u2(&mut out, 0); // minor_version
    push_u2(&mut out, SYNTHETIC_CLASS_MAJOR);
    push_u2(&mut out, pool.len() as u16);
    for constant in &pool[1..] {
        encode_constant(&mut out, constant)?;
    }
    push_u2(&mut out, ACC_PUBLIC | ACC_SUPER);
    push_u2(&mut out, this_class);
    push_u2(&mut out, super_class);
    push_u2(&mut out, 0); // interfaces
    push_u2(&mut out, 1); // fields
    push_synthetic_field(&mut out, ACC_PRIVATE, name_cancelled, desc_boolean);
    push_u2(&mut out, 3); // methods
    push_synthetic_method(
        &mut out,
        code_name,
        ACC_PUBLIC,
        name_init,
        desc_void,
        SyntheticCode {
            max_stack: 1,
            max_locals: 1,
            bytes: &init_bytes,
        },
    );
    push_synthetic_method(
        &mut out,
        code_name,
        ACC_PUBLIC,
        name_cancel,
        desc_void,
        SyntheticCode {
            max_stack: 2,
            max_locals: 1,
            bytes: &cancel_bytes,
        },
    );
    push_synthetic_method(
        &mut out,
        code_name,
        ACC_PUBLIC,
        name_is_cancelled,
        desc_is_cancelled,
        SyntheticCode {
            max_stack: 1,
            max_locals: 1,
            bytes: &is_cancelled_bytes,
        },
    );
    push_u2(&mut out, 0); // class attributes
    Ok(out)
}

/// The rewritten `Code` attribute of one method.
struct CodePatch<'a> {
    code: Vec<u8>,
    max_stack: u16,
    max_locals: u16,
    exception_table: Vec<ExceptionHandler>,
    attributes: Vec<CodeAttribute<'a>>,
}

struct Patch<'a> {
    method: usize,
    attribute: usize,
    code: CodePatch<'a>,
}

fn emit<'a>(
    layout: &Layout<'a>,
    patch: Option<Patch<'a>>,
    added: &[Cp],
) -> Result<Vec<u8>, String> {
    let pool_count = layout.pool_count as usize + added.len();
    let pool_count =
        u16::try_from(pool_count).map_err(|_| "constant pool has no free entries".to_owned())?;
    let map = PoolMap::identity(layout.pool_count);
    let mut out = Vec::with_capacity(layout.pool_entries.len() * 4 + 64);
    out.extend_from_slice(layout.version);
    out.extend_from_slice(&pool_count.to_be_bytes());
    layout.write_pool(&mut out, &map)?;
    for constant in added {
        encode_constant(&mut out, constant)?;
    }
    out.extend_from_slice(layout.after_pool);
    out.extend_from_slice(layout.fields);
    let method_count = u16::try_from(layout.methods.len())
        .map_err(|_| "class file has too many methods".to_owned())?;
    out.extend_from_slice(&method_count.to_be_bytes());
    for (method_index, method) in layout.methods.iter().enumerate() {
        out.extend_from_slice(&method.access.to_be_bytes());
        out.extend_from_slice(&method.name_index.to_be_bytes());
        out.extend_from_slice(&method.descriptor_index.to_be_bytes());
        let attribute_count = u16::try_from(method.attributes.len())
            .map_err(|_| "method has too many attributes".to_owned())?;
        out.extend_from_slice(&attribute_count.to_be_bytes());
        for (attribute_index, attribute) in method.attributes.iter().enumerate() {
            out.extend_from_slice(&attribute.name_index.to_be_bytes());
            if let Some(patch) = patch.as_ref()
                && patch.method == method_index
                && patch.attribute == attribute_index
            {
                let body = encode_code(&patch.code)?;
                let length = u32::try_from(body.len())
                    .map_err(|_| "Code attribute is longer than 4 GiB".to_owned())?;
                out.extend_from_slice(&length.to_be_bytes());
                out.extend_from_slice(&body);
                continue;
            }
            let length = u32::try_from(attribute.body.len())
                .map_err(|_| "attribute is longer than 4 GiB".to_owned())?;
            out.extend_from_slice(&length.to_be_bytes());
            out.extend_from_slice(attribute.body);
        }
    }
    layout.write_class_attributes(&mut out, &map)?;
    out.extend_from_slice(layout.tail);
    Ok(out)
}

fn encode_code(patch: &CodePatch<'_>) -> Result<Vec<u8>, String> {
    if patch.code.len() > MAX_CODE_LENGTH {
        return Err(format!(
            "method code is longer than the class-file limit of {MAX_CODE_LENGTH} bytes"
        ));
    }
    let length = u32::try_from(patch.code.len())
        .map_err(|_| "method code is longer than 4 GiB".to_owned())?;
    let mut body = Vec::with_capacity(patch.code.len() + patch.exception_table.len() * 8 + 64);
    body.extend_from_slice(&patch.max_stack.to_be_bytes());
    body.extend_from_slice(&patch.max_locals.to_be_bytes());
    body.extend_from_slice(&length.to_be_bytes());
    body.extend_from_slice(&patch.code);
    let handlers = u16::try_from(patch.exception_table.len())
        .map_err(|_| "exception table has too many entries".to_owned())?;
    body.extend_from_slice(&handlers.to_be_bytes());
    for handler in &patch.exception_table {
        body.extend_from_slice(&handler.start_pc.to_be_bytes());
        body.extend_from_slice(&handler.end_pc.to_be_bytes());
        body.extend_from_slice(&handler.handler_pc.to_be_bytes());
        body.extend_from_slice(&handler.catch_type.to_be_bytes());
    }
    let attributes = u16::try_from(patch.attributes.len())
        .map_err(|_| "Code attribute has too many attributes".to_owned())?;
    body.extend_from_slice(&attributes.to_be_bytes());
    for attribute in &patch.attributes {
        body.extend_from_slice(&attribute.name_index().to_be_bytes());
        let encoded = encode_code_attribute(attribute)?;
        let length = u32::try_from(encoded.len())
            .map_err(|_| "Code attribute is longer than 4 GiB".to_owned())?;
        body.extend_from_slice(&length.to_be_bytes());
        body.extend_from_slice(&encoded);
    }
    Ok(body)
}

fn encode_code_attribute(attribute: &CodeAttribute<'_>) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    match attribute {
        CodeAttribute::Raw { body, .. } => out.extend_from_slice(body),
        CodeAttribute::LineNumbers { lines, .. } => {
            let count = u16::try_from(lines.len())
                .map_err(|_| "LineNumberTable has too many entries".to_owned())?;
            out.extend_from_slice(&count.to_be_bytes());
            for line in lines {
                out.extend_from_slice(&line.start_pc.to_be_bytes());
                out.extend_from_slice(&line.line_number.to_be_bytes());
            }
        }
        CodeAttribute::LocalVariables { entries, .. } => {
            let count = u16::try_from(entries.len())
                .map_err(|_| "local-variable table has too many entries".to_owned())?;
            out.extend_from_slice(&count.to_be_bytes());
            for entry in entries {
                out.extend_from_slice(&entry.start_pc.to_be_bytes());
                out.extend_from_slice(&entry.length.to_be_bytes());
                out.extend_from_slice(&entry.name_index.to_be_bytes());
                out.extend_from_slice(&entry.descriptor_index.to_be_bytes());
                out.extend_from_slice(&entry.index.to_be_bytes());
            }
        }
        CodeAttribute::StackMap { frames, .. } => encode_frames(&mut out, frames)?,
    }
    Ok(out)
}

fn encode_frames(out: &mut Vec<u8>, frames: &[StackMapFrame]) -> Result<(), String> {
    let count =
        u16::try_from(frames.len()).map_err(|_| "StackMapTable has too many frames".to_owned())?;
    out.extend_from_slice(&count.to_be_bytes());
    let mut previous: Option<usize> = None;
    for frame in frames {
        let delta = match previous {
            None => frame.offset,
            Some(previous) => frame
                .offset
                .checked_sub(previous + 1)
                .ok_or_else(|| "stack-map frames are out of order".to_owned())?,
        };
        let delta = u16::try_from(delta).map_err(|_| {
            format!(
                "a stack-map frame would move past the class-file offset limit of {MAX_CODE_LENGTH}"
            )
        })?;
        previous = Some(frame.offset);
        match frame.frame_type {
            0..=63 => {
                if delta <= 63 {
                    out.push(delta as u8);
                } else {
                    out.push(251); // same_frame_extended
                    out.extend_from_slice(&delta.to_be_bytes());
                }
            }
            64..=127 => {
                if delta <= 63 {
                    out.push(64 + delta as u8);
                } else {
                    out.push(247); // same_locals_1_stack_item_frame_extended
                    out.extend_from_slice(&delta.to_be_bytes());
                }
                encode_verifications(out, &frame.stack);
            }
            247 => {
                out.push(247);
                out.extend_from_slice(&delta.to_be_bytes());
                encode_verifications(out, &frame.stack);
            }
            248..=251 => {
                out.push(frame.frame_type);
                out.extend_from_slice(&delta.to_be_bytes());
            }
            252..=254 => {
                out.push(frame.frame_type);
                out.extend_from_slice(&delta.to_be_bytes());
                encode_verifications(out, &frame.locals);
            }
            255 => {
                out.push(255);
                out.extend_from_slice(&delta.to_be_bytes());
                let locals = u16::try_from(frame.locals.len())
                    .map_err(|_| "stack-map frame has too many locals".to_owned())?;
                out.extend_from_slice(&locals.to_be_bytes());
                encode_verifications(out, &frame.locals);
                let stack = u16::try_from(frame.stack.len())
                    .map_err(|_| "stack-map frame has too many stack items".to_owned())?;
                out.extend_from_slice(&stack.to_be_bytes());
                encode_verifications(out, &frame.stack);
            }
            other => {
                return Err(format!(
                    "stack-map frame type {other} is reserved; the bounded weaver cannot re-encode it"
                ));
            }
        }
    }
    Ok(())
}

fn encode_verifications(out: &mut Vec<u8>, types: &[VerificationType]) {
    for verification in types {
        match verification {
            VerificationType::Simple(tag) => out.push(*tag),
            VerificationType::Object(index) => {
                out.push(7);
                out.extend_from_slice(&index.to_be_bytes());
            }
            VerificationType::Uninitialized(offset) => {
                out.push(8);
                out.extend_from_slice(&offset.to_be_bytes());
            }
        }
    }
}

/// Re-encode a class file from its parsed bytes.
///
/// Byte ranges the writer does not rewrite are copied verbatim, so an
/// unmodified class round-trips exactly.  A constant pool tag the writer does
/// not know is an error rather than an emitted byte stream that would not load.
pub(crate) fn write_class(raw: &[u8]) -> Result<Vec<u8>, String> {
    if raw.is_empty() {
        return Err("class file bytes are empty".to_owned());
    }
    let layout = Layout::parse(raw)?;
    emit(&layout, None, &[])
}

/// The load opcode and local-slot width for one argument descriptor.
fn load_opcode(descriptor: &str) -> Result<(u8, u16), String> {
    let Some(first) = descriptor.as_bytes().first() else {
        return Err("empty argument descriptor".to_owned());
    };
    Ok(match first {
        b'Z' | b'B' | b'C' | b'S' | b'I' => (0x15, 1), // iload
        b'J' => (0x16, 2),                             // lload
        b'F' => (0x17, 1),                             // fload
        b'D' => (0x18, 2),                             // dload
        b'L' | b'[' => (0x19, 1),                      // aload
        other => {
            return Err(format!(
                "argument descriptor {descriptor:?} starts with unsupported type {}",
                *other as char
            ));
        }
    })
}

fn opcode_name(opcode: u8) -> &'static str {
    match opcode {
        0x99 => "ifeq",
        0x9a => "ifne",
        0x9b => "iflt",
        0x9c => "ifge",
        0x9d => "ifgt",
        0x9e => "ifle",
        0x9f => "if_icmpeq",
        0xa0 => "if_icmpne",
        0xa1 => "if_icmplt",
        0xa2 => "if_icmpge",
        0xa3 => "if_icmpgt",
        0xa4 => "if_icmple",
        0xa5 => "if_acmpeq",
        0xa6 => "if_acmpne",
        0xa7 => "goto",
        0xa8 => "jsr",
        0xa9 => "ret",
        0xaa => "tableswitch",
        0xab => "lookupswitch",
        0xc4 => "wide",
        0xc6 => "ifnull",
        0xc7 => "ifnonnull",
        0xc8 => "goto_w",
        0xc9 => "jsr_w",
        _ => "unknown",
    }
}

/// A branch instruction that names a bytecode offset, and therefore has to be
/// rewritten when an insert moves the code after it.
struct BranchSite {
    /// Offset of the opcode in the original code.
    at: usize,
    /// The branch opcode, kept so a `goto`/`jsr` can be widened and an `if*`
    /// can be rejected by name.
    opcode: u8,
    /// Whether the offset field is 32 bits (`goto_w`/`jsr_w`).
    wide: bool,
    /// Absolute target offset in the original code.
    target: usize,
}

/// One `tableswitch`/`lookupswitch`, with every target as an absolute offset in
/// the original code.
struct SwitchSite {
    /// Offset of the opcode in the original code.
    at: usize,
    kind: SwitchKind,
}

enum SwitchKind {
    /// `tableswitch`: `targets[i]` answers `low + i`.
    Table {
        default: usize,
        low: i32,
        high: i32,
        targets: Vec<usize>,
    },
    /// `lookupswitch`: `pairs[i]` is `(match, target)`.
    Lookup {
        default: usize,
        pairs: Vec<(i32, usize)>,
    },
}

impl SwitchKind {
    fn name(&self) -> &'static str {
        match self {
            Self::Table { .. } => "tableswitch",
            Self::Lookup { .. } => "lookupswitch",
        }
    }

    fn opcode(&self) -> u8 {
        match self {
            Self::Table { .. } => 0xaa,
            Self::Lookup { .. } => 0xab,
        }
    }

    fn default(&self) -> usize {
        match self {
            Self::Table { default, .. } | Self::Lookup { default, .. } => *default,
        }
    }

    /// The `default` target followed by every case target.
    fn all_targets(&self) -> Vec<usize> {
        let mut targets = vec![self.default()];
        match self {
            Self::Table { targets: cases, .. } => targets.extend_from_slice(cases),
            Self::Lookup { pairs, .. } => {
                targets.extend(pairs.iter().map(|(_, target)| *target));
            }
        }
        targets
    }

    /// The encoded bytes after the opcode and its alignment padding.
    fn operand_len(&self) -> usize {
        match self {
            Self::Table { targets, .. } => 12 + 4 * targets.len(),
            Self::Lookup { pairs, .. } => 8 + 8 * pairs.len(),
        }
    }

    /// The instruction's length when its opcode sits at `at`, which is only
    /// known once the layout has placed it.
    fn len_at(&self, at: usize) -> usize {
        1 + switch_padding(at) + self.operand_len()
    }
}

/// The alignment padding a switch at `at` needs so its first operand starts on
/// a 4-byte boundary counted from the start of the `code` array.
fn switch_padding(at: usize) -> usize {
    (4 - (at + 1) % 4) % 4
}

/// One instruction of the original code.
struct Instruction {
    at: usize,
    size: usize,
    /// Index into [`CodeScan::switches`] when this instruction is a switch.
    switch: Option<usize>,
}

/// One pass over a method's code: its instruction boundaries, its branches, its
/// switches and its return instructions.
struct CodeScan {
    /// Instructions in file order, covering the code without gaps.
    instructions: Vec<Instruction>,
    branches: Vec<BranchSite>,
    switches: Vec<SwitchSite>,
    returns: Vec<usize>,
}

impl CodeScan {
    fn starts(&self) -> BTreeSet<usize> {
        self.instructions
            .iter()
            .map(|instruction| instruction.at)
            .collect()
    }
}

/// The byte size of one fixed-length instruction, or `None` for an opcode whose
/// size the caller has to work out (`tableswitch`, `lookupswitch`, `wide`).
fn instruction_size(opcode: u8) -> Option<usize> {
    Some(match opcode {
        0x00..=0x0f => 1,
        0x10 => 2,
        0x11 => 3,
        0x12 => 2,
        0x13 | 0x14 => 3,
        0x15..=0x19 => 2,
        0x1a..=0x35 => 1,
        0x36..=0x3a => 2,
        0x3b..=0x83 => 1,
        0x84 => 3,
        0x85..=0x98 => 1,
        0x99..=0xa8 => 3,
        0xa9 => 2,
        0xaa | 0xab | 0xc4 => return None,
        0xac..=0xb1 => 1,
        0xb2..=0xb8 => 3,
        0xb9 | 0xba => 5,
        0xbb => 3,
        0xbc => 2,
        0xbd => 3,
        0xbe | 0xbf => 1,
        0xc0 | 0xc1 => 3,
        0xc2 | 0xc3 => 1,
        0xc5 => 4,
        0xc6..=0xc9 => 3,
        _ => return None,
    })
}

/// The length of a `wide`-prefixed instruction at `at`: `wide` plus a two-byte
/// local index plus, for `iinc`, its two-byte constant.
fn wide_instruction_size(label: &str, code: &[u8], at: usize) -> Result<usize, String> {
    let sub = code
        .get(at + 1)
        .copied()
        .ok_or_else(|| format!("{label} has a truncated wide instruction at offset {at}"))?;
    match sub {
        // wide iload/lload/fload/dload/aload, the stores, and ret.
        0x15..=0x19 | 0x36..=0x3a | 0xa9 => Ok(4),
        // wide iinc <index> <const>.
        0x84 => Ok(6),
        other => Err(format!(
            "{label} uses wide opcode 0x{other:02x} at offset {at}; wide only prefixes a local-variable instruction"
        )),
    }
}

/// Parse one switch.  Bounds are checked against the code the switch lives in,
/// so a truncated table is a reason, not an allocation.
fn parse_switch(label: &str, code: &[u8], at: usize) -> Result<(SwitchSite, usize), String> {
    let opcode = code[at];
    let mut r = Bytes::new(code);
    r.at = at + 1 + switch_padding(at);
    let relative = |r: &mut Bytes<'_>| -> Result<usize, String> {
        let delta = r.u4()? as i32;
        let target = at as i64 + i64::from(delta);
        if target < 0 || target as usize >= code.len() {
            return Err(format!(
                "{} at offset {at} jumps to {target}, which is outside the {}-byte code",
                opcode_name(opcode),
                code.len()
            ));
        }
        Ok(target as usize)
    };
    let kind = match opcode {
        0xaa => {
            let default = relative(&mut r)?;
            let low = r.u4()? as i32;
            let high = r.u4()? as i32;
            let count = if i64::from(high) < i64::from(low) {
                if i64::from(high) == i64::from(low) - 1 {
                    0
                } else {
                    return Err(format!(
                        "tableswitch at offset {at} declares low {low} above high {high}"
                    ));
                }
            } else {
                let count = (i64::from(high) - i64::from(low) + 1) as u64;
                if count > (code.len() - r.at) as u64 / 4 {
                    return Err(format!(
                        "{label} has a tableswitch at offset {at} declaring {count} jump targets, more than the code holds"
                    ));
                }
                count as usize
            };
            let mut targets = Vec::with_capacity(count);
            for _ in 0..count {
                targets.push(relative(&mut r)?);
            }
            SwitchKind::Table {
                default,
                low,
                high,
                targets,
            }
        }
        0xab => {
            let default = relative(&mut r)?;
            let pairs = r.u4()? as i32;
            if pairs < 0 {
                return Err(format!(
                    "lookupswitch at offset {at} declares {pairs} match-offset pairs"
                ));
            }
            let pairs = pairs as usize;
            if pairs as u64 > (code.len() - r.at) as u64 / 8 {
                return Err(format!(
                    "{label} has a lookupswitch at offset {at} declaring {pairs} match-offset pairs, more than the code holds"
                ));
            }
            let mut entries = Vec::with_capacity(pairs);
            for _ in 0..pairs {
                let key = r.u4()? as i32;
                let target = relative(&mut r)?;
                entries.push((key, target));
            }
            SwitchKind::Lookup {
                default,
                pairs: entries,
            }
        }
        other => {
            return Err(format!(
                "opcode 0x{other:02x} at offset {at} is not a switch"
            ));
        }
    };
    let size = r.at - at;
    Ok((SwitchSite { at, kind }, size))
}

/// Walk a method's code, recording every instruction boundary, branch target,
/// switch target and return offset.
///
/// Every target has to name an instruction boundary: a relocation maps targets
/// through the layout, so a target inside an instruction would otherwise become
/// a branch into the middle of one.
fn scan_code(label: &str, code: &[u8]) -> Result<CodeScan, String> {
    let mut scan = CodeScan {
        instructions: Vec::new(),
        branches: Vec::new(),
        switches: Vec::new(),
        returns: Vec::new(),
    };
    let mut at = 0usize;
    while at < code.len() {
        let opcode = code[at];
        let (size, switch) = match opcode {
            0xaa | 0xab => {
                let (site, size) = parse_switch(label, code, at)?;
                scan.switches.push(site);
                (size, Some(scan.switches.len() - 1))
            }
            0xc4 => (wide_instruction_size(label, code, at)?, None),
            _ => {
                let size = instruction_size(opcode).ok_or_else(|| {
                    format!("{label} uses unknown or reserved opcode 0x{opcode:02x} at offset {at}")
                })?;
                (size, None)
            }
        };
        let end = at
            .checked_add(size)
            .ok_or_else(|| "code offset overflow".to_owned())?;
        if end > code.len() {
            return Err(format!(
                "{label} has a truncated instruction at offset {at}"
            ));
        }
        if matches!(opcode, 0xac..=0xb1) {
            scan.returns.push(at);
        }
        if let Some((wide, target)) = branch_target(opcode, code, at)? {
            scan.branches.push(BranchSite {
                at,
                opcode,
                wide,
                target,
            });
        }
        scan.instructions.push(Instruction { at, size, switch });
        at = end;
    }

    let starts = scan.starts();
    for branch in &scan.branches {
        if !starts.contains(&branch.target) {
            return Err(format!(
                "{label}: {} at offset {} targets {}, which is not an instruction boundary",
                opcode_name(branch.opcode),
                branch.at,
                branch.target
            ));
        }
    }
    for site in &scan.switches {
        for target in site.kind.all_targets() {
            if !starts.contains(&target) {
                return Err(format!(
                    "{label}: {} at offset {} targets {target}, which is not an instruction boundary",
                    site.kind.name(),
                    site.at
                ));
            }
        }
    }
    Ok(scan)
}

/// The signed relative offset of one branch instruction, or `None` when the
/// opcode has no bytecode offset operand.
fn branch_target(opcode: u8, code: &[u8], at: usize) -> Result<Option<(bool, usize)>, String> {
    let wide = match opcode {
        0x99..=0xa8 | 0xc6 | 0xc7 => false,
        0xc8 | 0xc9 => true,
        _ => return Ok(None),
    };
    let field = at + 1;
    let relative = if wide {
        let bytes = code
            .get(field..field + 4)
            .ok_or_else(|| format!("branch at offset {at} is truncated"))?;
        i64::from(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    } else {
        let bytes = code
            .get(field..field + 2)
            .ok_or_else(|| format!("branch at offset {at} is truncated"))?;
        i64::from(i16::from_be_bytes([bytes[0], bytes[1]]))
    };
    let target = at as i64 + relative;
    if target < 0 || target as usize >= code.len() {
        return Err(format!(
            "{} at offset {at} jumps to {target}, which is outside the {}-byte code",
            opcode_name(opcode),
            code.len()
        ));
    }
    Ok(Some((wide, target as usize)))
}

/// One relocation pass's answer: where every original instruction start, every
/// insert and the end of the code now sit.
struct CodePlan {
    /// New offset of each instruction start, keyed by its original offset.
    offsets: BTreeMap<usize, usize>,
    /// New offset where the bytes inserted before an instruction begin, i.e.
    /// the position an exclusive range end keeps the insert outside of.
    boundaries: BTreeMap<usize, usize>,
    /// Instructions that grew from `goto`/`jsr` to `goto_w`/`jsr_w`.
    widened: BTreeSet<usize>,
    /// Conditional branches that no longer reach their target in 16 bits and
    /// were split into an inverted branch over a `goto_w`.
    expanded: BTreeSet<usize>,
    /// New offset of the exclusive end of the code.
    end: usize,
    /// The original code's length, so an offset at the (exclusive) end maps
    /// without pretending it is an instruction.
    original_len: usize,
}

/// The length of the insert at `offset`, or `None`.  `insertions` is sorted by
/// offset, which [`relocate`] guarantees.
fn inserted_at(insertions: &[(usize, usize)], offset: usize) -> Option<usize> {
    insertions
        .binary_search_by_key(&offset, |(at, _)| *at)
        .ok()
        .map(|index| insertions[index].1)
}

/// Place every instruction of the original code in the rewritten buffer.
///
/// A switch's size is computed from the offset this pass gives it, so its
/// alignment padding is exact rather than carried over from the original
/// layout; an instruction in `widened` is five bytes instead of three.
fn layout_pass(
    label: &str,
    scan: &CodeScan,
    insertions: &[(usize, usize)],
    widened: &BTreeSet<usize>,
    expanded: &BTreeSet<usize>,
) -> Result<CodePlan, String> {
    let mut plan = CodePlan {
        offsets: BTreeMap::new(),
        boundaries: BTreeMap::new(),
        widened: widened.clone(),
        expanded: expanded.clone(),
        end: 0,
        original_len: scan
            .instructions
            .last()
            .map_or(0, |instruction| instruction.at + instruction.size),
    };
    let mut out = 0usize;
    for instruction in &scan.instructions {
        plan.boundaries.insert(instruction.at, out);
        out = out
            .checked_add(inserted_at(insertions, instruction.at).unwrap_or(0))
            .ok_or_else(|| format!("{label}: woven code length overflow"))?;
        plan.offsets.insert(instruction.at, out);
        let size = match instruction.switch {
            Some(index) => scan.switches[index].kind.len_at(out),
            None if expanded.contains(&instruction.at) => 8,
            None if widened.contains(&instruction.at) => 5,
            None => instruction.size,
        };
        out = out
            .checked_add(size)
            .ok_or_else(|| format!("{label}: woven code length overflow"))?;
    }
    plan.end = out;
    Ok(plan)
}

/// The opcode a wide branch has: `goto_w`/`jsr_w`.
fn widened_opcode(opcode: u8) -> Option<u8> {
    match opcode {
        0xa7 => Some(0xc8),
        0xa8 => Some(0xc9),
        _ => None,
    }
}

/// Relocate until the layout stops changing, bounded by
/// [`MAX_RELOCATION_PASSES`].
///
/// The only thing that can move between passes is a `goto`/`jsr` that had to
/// grow, because a grown instruction shifts every offset after it.  Widening is
/// monotone - a branch that fits stays narrow - so the loop cannot oscillate;
/// the bound exists to reject a method whose relocation does not settle instead
/// of spinning.
fn stable_plan(
    label: &str,
    scan: &CodeScan,
    insertions: &[(usize, usize)],
    entry: TargetEntry,
) -> Result<CodePlan, String> {
    let mut widened = BTreeSet::new();
    let mut expanded = BTreeSet::new();
    for _ in 0..MAX_RELOCATION_PASSES {
        let plan = layout_pass(label, scan, insertions, &widened, &expanded)?;
        let offset = |at: usize| {
            plan.offsets
                .get(&at)
                .copied()
                .ok_or_else(|| format!("{label}: offset {at} is not an instruction boundary"))
        };
        let mut grew = false;
        for branch in &scan.branches {
            let opcode_at = offset(branch.at)?;
            if expanded.contains(&branch.at) {
                // The branch is now `if<inverse> +8; goto_w target`; only the
                // wide jump's 32-bit operand has to fit.
                let target = map_target(label, &plan, branch.target, entry)?;
                if i32::try_from(target as i64 - (opcode_at as i64 + 3)).is_err() {
                    return Err(format!(
                        "{label}: the widened {} at offset {} would exceed the 32-bit branch range",
                        opcode_name(branch.opcode),
                        branch.at
                    ));
                }
                continue;
            }
            let delta = map_target(label, &plan, branch.target, entry)? as i64 - opcode_at as i64;
            if branch.wide {
                if i32::try_from(delta).is_err() {
                    return Err(format!(
                        "{label}: relocating the {} at offset {} would exceed the 32-bit branch range",
                        opcode_name(branch.opcode),
                        branch.at
                    ));
                }
                continue;
            }
            if i16::try_from(delta).is_ok() {
                continue;
            }
            if widened_opcode(branch.opcode).is_some() {
                if widened.insert(branch.at) {
                    grew = true;
                }
                continue;
            }
            if inverse_conditional(branch.opcode).is_none() {
                return Err(format!(
                    "{label}: {} at offset {} would sit {delta} bytes from its target after weaving, past the 16-bit branch range; the weaver only widens goto/jsr and splits the if* family",
                    opcode_name(branch.opcode),
                    branch.at
                ));
            }
            // `if*` has no wide encoding: overwrite it with the inverse
            // condition branching over a `goto_w`, which is 8 bytes instead of
            // 3.  The fallthrough instruction becomes the inverse branch's
            // target, so it needs a frame the relocation emits below.
            if !scan.starts().contains(&(branch.at + 3)) {
                return Err(format!(
                    "{label}: {} at offset {} has no fallthrough instruction to split around",
                    opcode_name(branch.opcode),
                    branch.at
                ));
            }
            if expanded.len() >= MAX_EXPANDED_BRANCHES {
                return Err(format!(
                    "{label}: more than {MAX_EXPANDED_BRANCHES} conditional branches would need splitting after weaving; the bounded weaver stops here"
                ));
            }
            if expanded.insert(branch.at) {
                grew = true;
            }
        }
        for site in &scan.switches {
            let opcode_at = plan.offsets.get(&site.at).copied().ok_or_else(|| {
                format!("{label}: offset {} is not an instruction boundary", site.at)
            })?;
            for target in site.kind.all_targets() {
                let target = map_target(label, &plan, target, entry)?;
                if i32::try_from(target as i64 - opcode_at as i64).is_err() {
                    return Err(format!(
                        "{label}: relocating the {} at offset {} would exceed the 32-bit switch range",
                        site.kind.name(),
                        site.at
                    ));
                }
            }
        }
        if !grew {
            return Ok(plan);
        }
    }
    Err(format!(
        "{label}: relocating switches and branches did not converge in {MAX_RELOCATION_PASSES} passes"
    ))
}

/// Where an original code offset now sits.
///
/// `inclusive` selects the two cases a rewrite distinguishes.  With `true` the
/// result is where the original byte at `offset` now sits: an insert exactly at
/// that offset goes *before* it, which is what an instruction, a branch target,
/// an exception handler, a stack-map frame, an `uninitialized` offset and a
/// debug-table start all name.  With `false` the result is the start of the
/// inserted block at `offset`, which is where an exclusive range end belongs so
/// the inserted bytes stay outside the range.
fn map_offset(
    label: &str,
    plan: &CodePlan,
    offset: usize,
    inclusive: bool,
) -> Result<usize, String> {
    if offset == plan.original_len {
        return Ok(plan.end);
    }
    let mapped = if inclusive {
        plan.offsets.get(&offset)
    } else {
        plan.boundaries.get(&offset)
    };
    mapped
        .copied()
        .ok_or_else(|| format!("{label}: offset {offset} is not an instruction boundary"))
}

fn shifted_u2(label: &str, value: usize, what: &str) -> Result<u16, String> {
    u16::try_from(value).map_err(|_| format!("{label}: {what} moves past the 65535-byte limit"))
}

/// Where a branch or switch target lands when it names an instruction that has
/// an insert in front of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetEntry {
    /// Keep the target on the original instruction, so the insert runs only
    /// when control falls into it.  `HEAD`'s insert sits at offset 0 and uses
    /// this: a backward branch to the method's first instruction must not run
    /// the callback a second time.
    Skip,
    /// Point the target at the inserted bytes, so every path that reaches the
    /// instruction runs the callback first.  `RETURN`/`TAIL` need this: a
    /// `return` that is also a branch or switch target - the body of a
    /// `switch` case, `try` handler tail, `if` arm - must still call back.
    Enter,
}

/// The new offset a branch or switch target maps to.
fn map_target(
    label: &str,
    plan: &CodePlan,
    offset: usize,
    entry: TargetEntry,
) -> Result<usize, String> {
    map_offset(label, plan, offset, entry == TargetEntry::Skip)
}

/// Write one switch at its planned offset, re-deriving its padding, its
/// relative `default`/case offsets and (for `tableswitch`) its `low`/`high`.
fn write_switch(
    label: &str,
    out: &mut Vec<u8>,
    site: &SwitchSite,
    plan: &CodePlan,
    entry: TargetEntry,
) -> Result<(), String> {
    let opcode_at = plan
        .offsets
        .get(&site.at)
        .copied()
        .ok_or_else(|| "a switch is missing from the layout".to_owned())?;
    if out.len() != opcode_at {
        return Err("a switch does not sit at its planned offset".to_owned());
    }
    let relative = |target: usize| -> Result<i32, String> {
        let target = map_target(label, plan, target, entry)?;
        i32::try_from(target as i64 - opcode_at as i64)
            .map_err(|_| "a relocated switch offset exceeds its 32-bit operand".to_owned())
    };
    out.push(site.kind.opcode());
    for _ in 0..switch_padding(opcode_at) {
        out.push(0);
    }
    match &site.kind {
        SwitchKind::Table {
            default,
            low,
            high,
            targets,
        } => {
            out.extend_from_slice(&relative(*default)?.to_be_bytes());
            out.extend_from_slice(&low.to_be_bytes());
            out.extend_from_slice(&high.to_be_bytes());
            for target in targets {
                out.extend_from_slice(&relative(*target)?.to_be_bytes());
            }
        }
        SwitchKind::Lookup { default, pairs } => {
            out.extend_from_slice(&relative(*default)?.to_be_bytes());
            let count = i32::try_from(pairs.len())
                .map_err(|_| "lookupswitch has too many match-offset pairs".to_owned())?;
            out.extend_from_slice(&count.to_be_bytes());
            for (key, target) in pairs {
                out.extend_from_slice(&key.to_be_bytes());
                out.extend_from_slice(&relative(*target)?.to_be_bytes());
            }
        }
    }
    Ok(())
}

/// Build the rewritten code from the stable plan: insert each callback at its
/// instruction, re-encode switches and branches, copy everything else.
fn emit_woven(
    label: &str,
    code: &[u8],
    scan: &CodeScan,
    plan: &CodePlan,
    insertions: &[(usize, usize)],
    bytes: &[u8],
    entry: TargetEntry,
) -> Result<Vec<u8>, String> {
    let branches = scan
        .branches
        .iter()
        .map(|branch| (branch.at, branch))
        .collect::<BTreeMap<usize, &BranchSite>>();
    let mut out = Vec::with_capacity(plan.end);
    for instruction in &scan.instructions {
        if inserted_at(insertions, instruction.at).is_some() {
            out.extend_from_slice(bytes);
        }
        if let Some(index) = instruction.switch {
            write_switch(label, &mut out, &scan.switches[index], plan, entry)?;
            continue;
        }
        let at = plan.offsets.get(&instruction.at).copied().ok_or_else(|| {
            format!(
                "{label}: instruction at {} is missing from the layout",
                instruction.at
            )
        })?;
        if out.len() != at {
            return Err(format!(
                "{label}: the woven layout disagrees at offset {}",
                instruction.at
            ));
        }
        let Some(branch) = branches.get(&instruction.at) else {
            out.extend_from_slice(&code[instruction.at..instruction.at + instruction.size]);
            continue;
        };
        let target = map_target(label, plan, branch.target, entry)?;
        let delta = target as i64 - at as i64;
        if plan.expanded.contains(&branch.at) {
            // `if<inverse> +8; goto_w target`: the inverse condition jumps over
            // the wide branch to the original fallthrough instruction, which
            // sits at `at + 8` (any insert in front of it begins there too).
            let inverse = inverse_conditional(branch.opcode).ok_or_else(|| {
                format!(
                    "{label}: {} at offset {} has no inverting conditional",
                    opcode_name(branch.opcode),
                    branch.at
                )
            })?;
            let fallthrough = map_target(label, plan, branch.at + 3, entry)?;
            let skip = i16::try_from(fallthrough as i64 - at as i64).map_err(|_| {
                format!(
                    "{label}: the split {} at offset {} cannot reach its fallthrough",
                    opcode_name(branch.opcode),
                    branch.at
                )
            })?;
            out.push(inverse);
            out.extend_from_slice(&skip.to_be_bytes());
            let wide_delta = i32::try_from(target as i64 - (at as i64 + 3)).map_err(|_| {
                format!("{label}: a widened branch exceeds the 32-bit branch range")
            })?;
            out.push(0xc8); // goto_w
            out.extend_from_slice(&wide_delta.to_be_bytes());
            continue;
        }
        match widened_opcode(branch.opcode).filter(|_| plan.widened.contains(&branch.at)) {
            Some(opcode) => {
                out.push(opcode);
                let delta = i32::try_from(delta).map_err(|_| {
                    format!("{label}: a widened branch exceeds the 32-bit branch range")
                })?;
                out.extend_from_slice(&delta.to_be_bytes());
            }
            None if branch.wide => {
                out.push(branch.opcode);
                let delta = i32::try_from(delta)
                    .map_err(|_| format!("{label}: a branch exceeds the 32-bit branch range"))?;
                out.extend_from_slice(&delta.to_be_bytes());
            }
            None => {
                out.push(branch.opcode);
                let delta = i16::try_from(delta).map_err(|_| {
                    format!(
                        "{label}: {} at offset {} exceeds the 16-bit branch range after weaving",
                        opcode_name(branch.opcode),
                        branch.at
                    )
                })?;
                out.extend_from_slice(&delta.to_be_bytes());
            }
        }
    }
    if out.len() != plan.end {
        return Err(format!(
            "{label}: the woven code is {} bytes, planned {}",
            out.len(),
            plan.end
        ));
    }
    Ok(out)
}

/// Check that a rewritten buffer is still a well-formed instruction stream.
///
/// The layout is the authority on where things moved; re-scanning is the cheap
/// independent check that every branch and switch target still lands on an
/// instruction boundary and every switch's padding agrees with its offset.
fn verify_woven(label: &str, code: &[u8]) -> Result<(), String> {
    scan_code(label, code)
        .map(|_| ())
        .map_err(|error| format!("{label}: the woven code does not re-scan: {error}"))
}

/// The insert sites, checked and sorted: every one carries the same bytes and
/// lands on an instruction boundary.
fn validate_insertions(
    label: &str,
    scan: &CodeScan,
    code_len: usize,
    insertions: &[(usize, usize)],
    length: usize,
) -> Result<Vec<(usize, usize)>, String> {
    let mut sorted = insertions.to_vec();
    sorted.sort_unstable_by_key(|(at, _)| *at);
    let starts = scan.starts();
    for (index, (at, insert)) in sorted.iter().enumerate() {
        if *insert != length {
            return Err(format!("{label}: inconsistent insert sites"));
        }
        if index > 0 && sorted[index - 1].0 == *at {
            return Err(format!("{label}: two inserts land at offset {at}"));
        }
        if *at >= code_len || !starts.contains(at) {
            return Err(format!(
                "{label}: an insert lands at offset {at}, which is not an instruction boundary"
            ));
        }
    }
    Ok(sorted)
}

/// Where the frames of a `Code` attribute whose offset carries an insert are
/// re-emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameSites {
    /// At the insert and at the instruction.  A `HEAD` insert and a `void`
    /// `RETURN` insert leave the operand stack and every local exactly as the
    /// verifier expects at the instruction, so its frame still describes the
    /// state after the insert and stays valid at both offsets.
    Both,
    /// At the insert only.  A non-void `RETURN` insert stores the target's
    /// result in a fresh local, so the state the verifier sees at the
    /// instruction differs by one live local: re-emitting the frame there would
    /// claim a `Top` slot that actually holds the boxed result.  The insert
    /// itself receives that frame, and every branch or switch that named the
    /// instruction now lands on the insert, so the instruction starts no basic
    /// block and needs none.
    InsertOnly,
}

/// A frame the rewrite synthesises where the original class had none.
///
/// A cancellable insert branches, so the instruction it jumps to becomes a
/// basic block and needs a verifier frame; a split conditional needs one for
/// its fallthrough.  The locals are slot-based (a wide value fills two slots)
/// and the stack is value-based, like the derivation that produces them.
#[derive(Clone)]
struct SyntheticFrame {
    offset: usize,
    locals: Vec<ValueType>,
    stack: Vec<ValueType>,
    /// `true` maps the offset to the instruction itself (after any insert at
    /// that offset); `false` maps it to the start of an insert there.
    at_instruction: bool,
}

/// Rewrite one method's code around a set of same-size inserts.
///
/// The layout pass places every instruction (and re-pads every switch) in the
/// new buffer; then every structure that names a bytecode offset is mapped
/// through it: branch operands, switch tables, exception table ranges,
/// `LineNumberTable`/`LocalVariableTable`/`LocalVariableTypeTable` `start_pc`,
/// `StackMapTable` frame deltas and `uninitialized` offsets.
#[allow(clippy::too_many_arguments)]
fn relocate<'a>(
    label: &str,
    target: &Target<'a>,
    code: &CodeLayout<'a>,
    scan: &CodeScan,
    insertions: &[(usize, usize)],
    bytes: &[u8],
    max_stack: u16,
    max_locals: u16,
    entry: TargetEntry,
    sites: FrameSites,
    extra_frames: &[SyntheticFrame],
    pool: &mut PoolBuilder<'_>,
) -> Result<CodePatch<'a>, String> {
    relocate_patched(
        label,
        target,
        code,
        scan,
        insertions,
        bytes,
        max_stack,
        max_locals,
        entry,
        sites,
        extra_frames,
        &[],
        None,
        pool,
    )
}

/// [`relocate`], plus fresh locals that a later `StackMapTable` frame has to
/// declare.
///
/// A frame's locals list is exact: a slot the insert writes holds that type for
/// the rest of the method, so a frame that omitted it would claim `Top` where
/// the verifier sees a value.  Patching the frames forces the full-frame
/// encoding, because a compressed frame is a delta from the one in front of it.
#[allow(clippy::too_many_arguments)]
fn relocate_patched<'a>(
    label: &str,
    target: &Target<'a>,
    code: &CodeLayout<'a>,
    scan: &CodeScan,
    insertions: &[(usize, usize)],
    bytes: &[u8],
    max_stack: u16,
    max_locals: u16,
    entry: TargetEntry,
    sites: FrameSites,
    extra_frames: &[SyntheticFrame],
    frame_locals: &[FrameLocalPatch],
    replace: Option<(usize, usize)>,
    pool: &mut PoolBuilder<'_>,
) -> Result<CodePatch<'a>, String> {
    let insertions = validate_insertions(label, scan, code.code.len(), insertions, bytes.len())?;
    let plan = stable_plan(label, scan, &insertions, entry)?;
    let mut woven = emit_woven(label, code.code, scan, &plan, &insertions, bytes, entry)?;
    if let Some((at, length)) = replace {
        let mapped = map_offset(label, &plan, at, true)?;
        let end = mapped.checked_add(length).ok_or_else(|| {
            format!("{label}: the replaced instruction overflows the code length")
        })?;
        if end > woven.len() {
            return Err(format!(
                "{label}: the replaced instruction at {at} does not fit the woven code"
            ));
        }
        for byte in &mut woven[mapped..end] {
            *byte = 0x00; // nop: the insert already did the access's work
        }
    }
    verify_woven(label, &woven)?;

    let mut exception_table = Vec::with_capacity(code.exception_table.len());
    for handler in &code.exception_table {
        // The range keeps the original instructions: an insert exactly at the
        // start lands before it, an insert exactly at the exclusive end lands
        // after it, so the callback is never caught by the target's own handler.
        let (start, end) = if handler.start_pc == handler.end_pc {
            (
                map_offset(label, &plan, handler.start_pc as usize, false)?,
                map_offset(label, &plan, handler.end_pc as usize, false)?,
            )
        } else {
            (
                map_offset(label, &plan, handler.start_pc as usize, true)?,
                map_offset(label, &plan, handler.end_pc as usize, false)?,
            )
        };
        let handler_pc = map_target(label, &plan, handler.handler_pc as usize, entry)?;
        if start > end || handler_pc >= woven.len() {
            return Err(format!(
                "{label}: relocating the exception table produced an invalid range"
            ));
        }
        exception_table.push(ExceptionHandler {
            start_pc: shifted_u2(label, start, "an exception range")?,
            end_pc: shifted_u2(label, end, "an exception range")?,
            handler_pc: shifted_u2(label, handler_pc, "an exception handler")?,
            catch_type: handler.catch_type,
        });
    }

    // Frames the rewrite introduces that no original frame describes: the
    // callers' (a cancellable `HEAD` body, an INVOKE call and its
    // continuation) plus the fallthrough of every conditional the layout had
    // to split.  A class file below version 50 ignores `StackMapTable` and is
    // verified by type inference, so it needs none of them.
    let mut extras = extra_frames.to_vec();
    for at in &plan.expanded {
        let fallthrough = at + 3;
        if code_frames_have(code, fallthrough) {
            continue;
        }
        let state = derive_state_at(label, target, code, scan, fallthrough)?;
        extras.push(SyntheticFrame {
            offset: fallthrough,
            locals: state.locals,
            stack: Vec::new(),
            at_instruction: entry == TargetEntry::Skip,
        });
    }
    if target.major() < 50 {
        extras.clear();
    }
    let mut seen = BTreeSet::new();
    extras.retain(|frame| !code_frames_have(code, frame.offset) && seen.insert(frame.offset));

    let mut attributes = Vec::with_capacity(code.attributes.len() + 1);
    let mut has_stack_map = false;
    for attribute in &code.attributes {
        match attribute {
            CodeAttribute::StackMap { name_index, frames } => {
                has_stack_map = true;
                let frames = if extras.is_empty() && frame_locals.is_empty() {
                    relocate_frames(label, frames, &plan, &insertions, sites)?
                } else {
                    relocate_frames_full(
                        label,
                        target,
                        frames,
                        &plan,
                        &insertions,
                        sites,
                        &extras,
                        frame_locals,
                        pool,
                    )?
                };
                attributes.push(CodeAttribute::StackMap {
                    name_index: *name_index,
                    frames,
                });
            }
            other => {
                attributes.push(relocate_attribute(label, other, &plan)?);
            }
        }
    }
    if !has_stack_map && !extras.is_empty() {
        let name_index = pool.utf8("StackMapTable")?;
        attributes.push(CodeAttribute::StackMap {
            name_index,
            frames: synthesize_frames(label, &extras, &plan, pool)?,
        });
    }

    Ok(CodePatch {
        code: woven,
        max_stack,
        max_locals,
        exception_table,
        attributes,
    })
}

/// Whether the method already carries a stack-map frame at `offset`.
fn code_frames_have(code: &CodeLayout<'_>, offset: usize) -> bool {
    code.attributes.iter().any(|attribute| match attribute {
        CodeAttribute::StackMap { frames, .. } => frames.iter().any(|frame| frame.offset == offset),
        _ => false,
    })
}

fn relocate_attribute<'a>(
    label: &str,
    attribute: &CodeAttribute<'a>,
    plan: &CodePlan,
) -> Result<CodeAttribute<'a>, String> {
    Ok(match attribute {
        CodeAttribute::Raw { name_index, body } => CodeAttribute::Raw {
            name_index: *name_index,
            body,
        },
        CodeAttribute::LineNumbers { name_index, lines } => CodeAttribute::LineNumbers {
            name_index: *name_index,
            lines: lines
                .iter()
                .map(|line| {
                    Ok(LineNumber {
                        start_pc: shifted_u2(
                            label,
                            map_offset(label, plan, line.start_pc as usize, true)?,
                            "a line number",
                        )?,
                        line_number: line.line_number,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
        },
        CodeAttribute::LocalVariables {
            name_index,
            entries,
        } => CodeAttribute::LocalVariables {
            name_index: *name_index,
            entries: entries
                .iter()
                .map(|entry| relocate_local(label, entry, plan))
                .collect::<Result<Vec<_>, String>>()?,
        },
        CodeAttribute::StackMap { .. } => {
            return Err(format!(
                "{label}: a stack map is relocated by the frame pass, not by itself"
            ));
        }
    })
}

fn relocate_local(
    label: &str,
    entry: &LocalVariable,
    plan: &CodePlan,
) -> Result<LocalVariable, String> {
    let start = map_offset(label, plan, entry.start_pc as usize, true)?;
    let end = map_offset(
        label,
        plan,
        entry.start_pc as usize + entry.length as usize,
        false,
    )?;
    if end < start {
        return Err(format!(
            "{label}: relocating a local-variable range produced an invalid range"
        ));
    }
    Ok(LocalVariable {
        start_pc: shifted_u2(label, start, "a local-variable range")?,
        length: shifted_u2(label, end - start, "a local-variable range")?,
        name_index: entry.name_index,
        descriptor_index: entry.descriptor_index,
        index: entry.index,
    })
}

fn relocate_frames(
    label: &str,
    frames: &[StackMapFrame],
    plan: &CodePlan,
    insertions: &[(usize, usize)],
    sites: FrameSites,
) -> Result<Vec<StackMapFrame>, String> {
    let mut out = Vec::with_capacity(frames.len() * 2);
    for frame in frames {
        // An insert at a frame position puts the callback in front of the
        // instruction that frame describes, which leaves the callback where the
        // verifier still wants a frame: the instruction after an unconditional
        // transfer (the `goto`/switch that named this position) starts a basic
        // block even when nothing branches to the inserted bytes.  A `HEAD` or
        // void `RETURN` callback runs in exactly the frame's state - it only
        // pushes its arguments and calls `invokestatic` - so the same frame is
        // valid at both offsets.  A non-void `RETURN` insert leaves one more live
        // local behind, so only the insert keeps the frame.
        if inserted_at(insertions, frame.offset).is_some()
            && (frame.offset > 0 || sites == FrameSites::InsertOnly)
        {
            let boundary = map_offset(label, plan, frame.offset, false)?;
            push_frame(label, &mut out, frame, boundary, plan)?;
            if sites == FrameSites::InsertOnly {
                continue;
            }
        }
        let offset = map_offset(label, plan, frame.offset, true)?;
        push_frame(label, &mut out, frame, offset, plan)?;
    }
    Ok(out)
}

/// Append one relocated frame, keeping the table strictly increasing.
fn push_frame(
    label: &str,
    out: &mut Vec<StackMapFrame>,
    frame: &StackMapFrame,
    offset: usize,
    plan: &CodePlan,
) -> Result<(), String> {
    if let Some(previous) = out.last() {
        let previous: &StackMapFrame = previous;
        if offset <= previous.offset {
            return Err(format!(
                "{label}: relocating stack-map frames produced a duplicate"
            ));
        }
    }
    out.push(StackMapFrame {
        frame_type: frame.frame_type,
        offset,
        locals: frame
            .locals
            .iter()
            .map(|verification| relocate_verification(label, verification, plan))
            .collect::<Result<Vec<_>, String>>()?,
        stack: frame
            .stack
            .iter()
            .map(|verification| relocate_verification(label, verification, plan))
            .collect::<Result<Vec<_>, String>>()?,
    });
    Ok(())
}

/// Relocate a stack map as a table of full frames, so a frame this rewrite
/// synthesises can be inserted without changing what a later delta frame means.
///
/// Compressed frames (`same`, `append`, `chop`) are deltas from the previous
/// frame.  Inserting a frame in the middle would make a following `append`
/// append to the new frame instead of the original one, so every frame in this
/// path is written as a `full_frame` with its absolute locals and stack.
#[allow(clippy::too_many_arguments)]
fn relocate_frames_full(
    label: &str,
    target: &Target<'_>,
    frames: &[StackMapFrame],
    plan: &CodePlan,
    insertions: &[(usize, usize)],
    sites: FrameSites,
    extras: &[SyntheticFrame],
    frame_locals: &[FrameLocalPatch],
    pool: &mut PoolBuilder<'_>,
) -> Result<Vec<StackMapFrame>, String> {
    let full = full_frames(target, frames)?;
    let mut out = Vec::with_capacity(full.len() * 2 + extras.len());
    for (offset, locals, stack) in &full {
        let mut locals = locals.clone();
        for patch in frame_locals {
            if *offset > patch.after {
                set_frame_local(&mut locals, patch.slot, &patch.value);
            }
        }
        if inserted_at(insertions, *offset).is_some()
            && (*offset > 0 || sites == FrameSites::InsertOnly)
        {
            let boundary = map_offset(label, plan, *offset, false)?;
            out.push(full_frame(label, boundary, &locals, stack, plan, pool)?);
            if sites == FrameSites::InsertOnly {
                continue;
            }
        }
        let mapped = map_offset(label, plan, *offset, true)?;
        out.push(full_frame(label, mapped, &locals, stack, plan, pool)?);
    }
    for extra in extras {
        let mapped = if extra.at_instruction {
            map_offset(label, plan, extra.offset, true)?
        } else {
            map_offset(label, plan, extra.offset, false)?
        };
        let locals = values_from_slots(&extra.locals);
        out.push(full_frame(
            label,
            mapped,
            &locals,
            &extra.stack,
            plan,
            pool,
        )?);
    }
    sort_frames(label, out)
}

/// The frames for a method that had no `StackMapTable`, built only from the
/// extras this rewrite introduces.
fn synthesize_frames(
    label: &str,
    extras: &[SyntheticFrame],
    plan: &CodePlan,
    pool: &mut PoolBuilder<'_>,
) -> Result<Vec<StackMapFrame>, String> {
    let mut out = Vec::with_capacity(extras.len());
    for extra in extras {
        let mapped = if extra.at_instruction {
            map_offset(label, plan, extra.offset, true)?
        } else {
            map_offset(label, plan, extra.offset, false)?
        };
        let locals = values_from_slots(&extra.locals);
        out.push(full_frame(
            label,
            mapped,
            &locals,
            &extra.stack,
            plan,
            pool,
        )?);
    }
    sort_frames(label, out)
}

fn full_frame(
    label: &str,
    offset: usize,
    locals: &[ValueType],
    stack: &[ValueType],
    plan: &CodePlan,
    pool: &mut PoolBuilder<'_>,
) -> Result<StackMapFrame, String> {
    Ok(StackMapFrame {
        frame_type: 255,
        offset,
        locals: locals
            .iter()
            .map(|value| verification_from_value(label, value, plan, pool))
            .collect::<Result<Vec<_>, _>>()?,
        stack: stack
            .iter()
            .map(|value| verification_from_value(label, value, plan, pool))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn sort_frames(label: &str, mut frames: Vec<StackMapFrame>) -> Result<Vec<StackMapFrame>, String> {
    frames.sort_by_key(|frame| frame.offset);
    for pair in frames.windows(2) {
        if pair[0].offset == pair[1].offset {
            return Err(format!(
                "{label}: two stack-map frames land at offset {}",
                pair[0].offset
            ));
        }
    }
    Ok(frames)
}

fn relocate_verification(
    label: &str,
    verification: &VerificationType,
    plan: &CodePlan,
) -> Result<VerificationType, String> {
    Ok(match verification {
        VerificationType::Simple(tag) => VerificationType::Simple(*tag),
        VerificationType::Object(index) => VerificationType::Object(*index),
        VerificationType::Uninitialized(offset) => VerificationType::Uninitialized(shifted_u2(
            label,
            map_offset(label, plan, *offset as usize, true)?,
            "an uninitialized offset",
        )?),
    })
}

fn find_target(class_name: &str, layout: &Layout<'_>, target: &MethodRef) -> Result<usize, String> {
    let matches = layout
        .methods
        .iter()
        .enumerate()
        .filter(|(_, method)| {
            layout.method_name(method) == Some(target.name.as_str())
                && (target.descriptor.is_empty()
                    || layout.method_descriptor(method) == Some(target.descriptor.as_str()))
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(format!(
            "class {class_name} has no method {}{}",
            target.name, target.descriptor
        )),
        [index] => Ok(*index),
        many => {
            let mut names = many
                .iter()
                .filter_map(|index| layout.method_descriptor(&layout.methods[*index]))
                .collect::<Vec<_>>();
            names.sort_unstable();
            Err(format!(
                "class {class_name} target {}{} is ambiguous: it declares {}",
                target.name,
                target.descriptor,
                names.join(", ")
            ))
        }
    }
}

/// The object one weave shape appends as the callback's last argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Injected {
    /// No injected object: a `HEAD` or `void` `RETURN`/`TAIL` callback.
    None,
    /// The `CallbackInfoReturnable` a non-void `RETURN`/`TAIL` insert builds.
    Returnable,
    /// The `CallbackInfo` a cancellable `void` `HEAD` insert builds.
    Cancellable,
}

/// The callback shape one insert emits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CallbackShape {
    /// The callback takes the target instance as an extra first parameter,
    /// which the inserted `aload_0` supplies.
    takes_receiver: bool,
    /// The callback ends with a plain `CallbackInfo`, which the insert
    /// constructs before the call and passes as the last argument.
    callback_info: bool,
}

/// Check a callback descriptor against the weave shape.
///
/// Returns the loads the insert has to push.  A static target's callback must
/// take exactly the target's arguments; an instance target's callback may take
/// them with or without the receiver.
///
/// `injected` names the object the callback must end with, if any: the
/// `CallbackInfoReturnable` a non-void return insert builds, or the
/// `CallbackInfo` a cancellable `void` `HEAD` insert builds.
///
/// A non-cancellable insert also accepts a trailing plain `CallbackInfo`: it
/// builds [`callback_info_class`] and passes it, which is the object real Mixin
/// hands every `@Inject` callback.  Nothing reads `isCancelled` on that object,
/// so a `cancel()` from a non-cancellable callback changes nothing.
fn callback_arguments(
    class_name: &str,
    is_static: bool,
    target_descriptor: &str,
    callback_descriptor: &str,
    anchor: &str,
    injected: Injected,
) -> Result<CallbackShape, String> {
    if !crate::valid_method_descriptor(callback_descriptor) {
        return Err(format!(
            "callback descriptor {callback_descriptor:?} is not a method descriptor"
        ));
    }
    let callback_arguments = crate::descriptor_arguments(callback_descriptor);
    let last = callback_arguments.last().map(String::as_str);
    let (trailing_desc, callback_info) = match injected {
        Injected::None => match last {
            Some(last) if last == CALLBACK_INFO_DESC => (Some(CALLBACK_INFO_DESC), true),
            Some(last) if last == CALLBACK_INFO_RETURNABLE_DESC => {
                return Err(format!(
                    "callback {callback_descriptor} takes a trailing CallbackInfoReturnable; only a non-void RETURN/TAIL insert carries a result"
                ));
            }
            _ => (None, false),
        },
        Injected::Returnable => {
            if last != Some(CALLBACK_INFO_RETURNABLE_DESC) {
                let detail = if last.is_some_and(crate::is_callback_info_descriptor) {
                    "a plain CallbackInfo cannot carry the result"
                } else {
                    "the callback declares no such argument"
                };
                return Err(format!(
                    "{anchor} weaving a non-void target needs the callback's last argument to be {CALLBACK_INFO_RETURNABLE_DESC}: the insert boxes the target's result into that object and returns getReturnValue(), so no override branch and no synthesised StackMapTable frame is needed; {detail}, so {callback_descriptor} cannot observe or replace the result and stays on the interpreter path"
                ));
            }
            (Some(CALLBACK_INFO_RETURNABLE_DESC), false)
        }
        Injected::Cancellable => {
            if last != Some(CALLBACK_INFO_DESC) {
                let detail = if last == Some(CALLBACK_INFO_RETURNABLE_DESC) {
                    "a CallbackInfoReturnable belongs on a non-void RETURN/TAIL anchor"
                } else {
                    "the callback declares no CallbackInfo argument"
                };
                return Err(format!(
                    "{anchor} weaving a cancellable void target needs the callback's last argument to be {CALLBACK_INFO_DESC}, which the insert constructs and cancels with; {detail}, so {callback_descriptor} cannot cancel and stays on the interpreter path"
                ));
            }
            (Some(CALLBACK_INFO_DESC), true)
        }
    };
    if crate::return_desc(callback_descriptor) != "V" {
        return Err(format!(
            "callback {callback_descriptor} must return void for {anchor} weaving"
        ));
    }
    let target_arguments = crate::descriptor_arguments(target_descriptor);
    let descriptor = |receiver: Option<&str>| {
        let mut shape = String::from("(");
        if let Some(receiver) = receiver {
            shape.push('L');
            shape.push_str(receiver);
            shape.push(';');
        }
        for argument in &target_arguments {
            shape.push_str(argument);
        }
        if let Some(trailing) = trailing_desc {
            shape.push_str(trailing);
        }
        shape.push_str(")V");
        shape
    };
    let plain = descriptor(None);
    if callback_descriptor == plain {
        return Ok(CallbackShape {
            takes_receiver: false,
            callback_info,
        });
    }
    if is_static {
        return Err(format!(
            "static target expects callback {plain}, got {callback_descriptor}"
        ));
    }
    let with_receiver = descriptor(Some(class_name));
    if callback_descriptor == with_receiver {
        return Ok(CallbackShape {
            takes_receiver: true,
            callback_info,
        });
    }
    Err(format!(
        "callback descriptor {callback_descriptor} does not match the woven arguments {plain} or {with_receiver}"
    ))
}

/// A target method prepared for weaving: its parsed layout, the index of the
/// `Code` attribute every anchor rewrites, and the descriptor facts the
/// callback checks need.
struct Target<'a> {
    layout: Layout<'a>,
    class_name: String,
    /// The parsed constant pool, so the verifier-state derivation can resolve
    /// field and method descriptors the code names.
    pool: &'a [Cp],
    method_index: usize,
    attribute_index: usize,
    name: String,
    descriptor: String,
    is_static: bool,
}

impl<'a> Target<'a> {
    fn label(&self) -> String {
        format!("{}.{}{}", self.class_name, self.name, self.descriptor)
    }

    /// The class-file major version, which decides whether a `StackMapTable`
    /// is authoritative (50 and up) or ignored (49 and below).
    fn major(&self) -> u16 {
        u16::from_be_bytes([self.layout.version[6], self.layout.version[7]])
    }

    fn code(&self) -> Result<&CodeLayout<'a>, String> {
        self.layout.methods[self.method_index]
            .attributes
            .get(self.attribute_index)
            .and_then(|attribute| attribute.code.as_ref())
            .ok_or_else(|| format!("{}: method has no Code attribute", self.label()))
    }
}

/// Shared validation for every anchor: owner match, condy, the target method,
/// its `Code` attribute, and the opcodes no insert can keep valid.
fn prepare_target<'a>(class: &'a ClassFile, target: &MethodRef) -> Result<Target<'a>, String> {
    if target.owner != class.name {
        return Err(format!(
            "target owner {} does not match class {}",
            target.owner, class.name
        ));
    }
    let layout = Layout::parse(&class.raw)?;
    layout
        .validate_constant_pool()
        .map_err(|error| format!("{}.{}: {error}", class.name, target.name))?;
    let method_index = find_target(&class.name, &layout, target)?;
    let method = &layout.methods[method_index];
    let descriptor = layout
        .method_descriptor(method)
        .ok_or_else(|| format!("{}: target has no readable descriptor", class.name))?
        .to_owned();
    let name = layout
        .method_name(method)
        .ok_or_else(|| format!("{}: target has no readable name", class.name))?
        .to_owned();
    if !crate::valid_method_descriptor(&descriptor) {
        return Err(format!(
            "{}.{name}: target descriptor {descriptor:?} is not a method descriptor",
            class.name
        ));
    }
    if method.access & (ACC_ABSTRACT | ACC_NATIVE) != 0 {
        return Err(format!(
            "{}.{name}{descriptor}: abstract or native method has no Code to weave",
            class.name
        ));
    }
    let attribute_index = method
        .attributes
        .iter()
        .position(|attribute| attribute.code.is_some())
        .ok_or_else(|| {
            format!(
                "{}.{name}{descriptor}: method has no Code attribute",
                class.name
            )
        })?;
    let code = method.attributes[attribute_index]
        .code
        .as_ref()
        .ok_or_else(|| "missing Code attribute".to_owned())?;

    // Reject the opcodes no insert can keep valid before any plan runs.
    let label = format!("{}.{name}{descriptor}", class.name);
    scan_code(&label, code.code)?;

    let is_static = method.access & ACC_STATIC != 0;
    Ok(Target {
        layout,
        class_name: class.name.clone(),
        pool: &class.pool,
        method_index,
        attribute_index,
        name,
        descriptor,
        is_static,
    })
}

/// The JVM type model this module needs to re-derive one `StackMapTable`
/// frame: enough to follow the local-variable types along a straight-line
/// region, not a full verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ValueType {
    Top,
    Int,
    Float,
    Long,
    Double,
    Null,
    UninitializedThis,
    /// An initialized reference, by internal name or array descriptor.
    Object(String),
    /// An object whose `new` sits at this original bytecode offset and whose
    /// constructor has not run yet.
    Uninitialized(usize),
}

impl ValueType {
    fn width(&self) -> usize {
        match self {
            Self::Long | Self::Double => 2,
            _ => 1,
        }
    }

    /// The type one field or argument descriptor names.
    fn from_descriptor(descriptor: &str) -> Result<Self, String> {
        match descriptor.as_bytes().first().copied() {
            Some(b'I' | b'Z' | b'B' | b'C' | b'S') => Ok(Self::Int),
            Some(b'J') => Ok(Self::Long),
            Some(b'F') => Ok(Self::Float),
            Some(b'D') => Ok(Self::Double),
            Some(b'L') if descriptor.ends_with(';') && descriptor.len() > 2 => {
                Ok(Self::Object(descriptor[1..descriptor.len() - 1].to_owned()))
            }
            Some(b'[') => Ok(Self::Object(descriptor.to_owned())),
            _ => Err(format!("descriptor {descriptor:?} is not a value type")),
        }
    }
}

fn pool_utf8_at(pool: &[Cp], index: u16) -> Option<&str> {
    match pool.get(index as usize) {
        Some(Cp::Utf8(value)) => Some(value.as_str()),
        _ => None,
    }
}

fn pool_class_at(pool: &[Cp], index: u16) -> Option<&str> {
    match pool.get(index as usize) {
        Some(Cp::Class(name)) => pool_utf8_at(pool, *name),
        _ => None,
    }
}

fn pool_name_type_at(pool: &[Cp], index: u16) -> Option<(&str, &str)> {
    match pool.get(index as usize) {
        Some(Cp::NameType { name, descriptor }) => {
            Some((pool_utf8_at(pool, *name)?, pool_utf8_at(pool, *descriptor)?))
        }
        _ => None,
    }
}

fn pool_field_type(pool: &[Cp], index: u16) -> Option<&str> {
    match pool.get(index as usize) {
        Some(Cp::FieldRef { nt, .. }) => {
            pool_name_type_at(pool, *nt).map(|(_, descriptor)| descriptor)
        }
        _ => None,
    }
}

fn pool_method(pool: &[Cp], index: u16) -> Option<(&str, &str)> {
    match pool.get(index as usize) {
        Some(Cp::MethodRef { nt, .. } | Cp::InterfaceMethodRef { nt, .. }) => {
            pool_name_type_at(pool, *nt)
        }
        _ => None,
    }
}

/// The owner, name and descriptor one `Methodref`/`InterfaceMethodref` names.
fn invoke_method_ref(pool: &[Cp], index: u16) -> Option<(&str, &str, &str)> {
    match pool.get(index as usize) {
        Some(Cp::MethodRef { class, nt } | Cp::InterfaceMethodRef { class, nt }) => {
            let owner = pool_class_at(pool, *class)?;
            let (name, descriptor) = pool_name_type_at(pool, *nt)?;
            Some((owner, name, descriptor))
        }
        _ => None,
    }
}

fn pool_invoke_dynamic_descriptor(pool: &[Cp], index: u16) -> Option<&str> {
    match pool.get(index as usize) {
        Some(Cp::InvokeDynamic { nt, .. }) => {
            pool_name_type_at(pool, *nt).map(|(_, descriptor)| descriptor)
        }
        _ => None,
    }
}

/// The local-variable types a method starts with, as a `StackMapTable` locals
/// list (wide values take one entry).
fn initial_local_values(target: &Target<'_>) -> Result<Vec<ValueType>, String> {
    let mut values = Vec::new();
    if !target.is_static {
        values.push(if target.name == "<init>" {
            ValueType::UninitializedThis
        } else {
            ValueType::Object(target.class_name.clone())
        });
    }
    for argument in crate::descriptor_arguments(&target.descriptor) {
        values.push(ValueType::from_descriptor(&argument)?);
    }
    Ok(values)
}

/// Expand a `StackMapTable` locals list (one entry per value) to one entry per
/// local slot, which is how the derivation stores and reads it.
fn slots_from_values(values: &[ValueType]) -> Vec<ValueType> {
    let mut slots = Vec::new();
    for value in values {
        slots.push(value.clone());
        if value.width() == 2 {
            slots.push(ValueType::Top);
        }
    }
    slots
}

/// Collapse one entry per local slot back to a `StackMapTable` locals list,
/// dropping the trailing `Top`s the table leaves implicit.
fn values_from_slots(slots: &[ValueType]) -> Vec<ValueType> {
    let mut values = Vec::new();
    let mut at = 0;
    while at < slots.len() {
        let value = slots[at].clone();
        let width = value.width();
        values.push(value);
        at += width;
    }
    while values.last() == Some(&ValueType::Top) {
        values.pop();
    }
    values
}

/// Declare one fresh local in a frame's locals list, padding the slots before
/// it with `Top`.
fn set_frame_local(locals: &mut Vec<ValueType>, slot: u16, value: &ValueType) {
    let mut slots = slots_from_values(locals);
    let slot = slot as usize;
    while slots.len() < slot + value.width() {
        slots.push(ValueType::Top);
    }
    slots[slot] = value.clone();
    if value.width() == 2 {
        slots[slot + 1] = ValueType::Top;
    }
    *locals = values_from_slots(&slots);
}

fn value_from_verification(
    pool: &[Cp],
    verification: &VerificationType,
) -> Result<ValueType, String> {
    Ok(match verification {
        VerificationType::Simple(0) => ValueType::Top,
        VerificationType::Simple(1) => ValueType::Int,
        VerificationType::Simple(2) => ValueType::Float,
        VerificationType::Simple(3) => ValueType::Double,
        VerificationType::Simple(4) => ValueType::Long,
        VerificationType::Simple(5) => ValueType::Null,
        VerificationType::Simple(6) => ValueType::UninitializedThis,
        VerificationType::Simple(other) => {
            return Err(format!("stack-map type {other} is reserved"));
        }
        VerificationType::Object(index) => ValueType::Object(
            pool_class_at(pool, *index)
                .ok_or_else(|| format!("stack-map object index {index} is not a class"))?
                .to_owned(),
        ),
        VerificationType::Uninitialized(offset) => ValueType::Uninitialized(*offset as usize),
    })
}

fn verification_from_value(
    label: &str,
    value: &ValueType,
    plan: &CodePlan,
    pool: &mut PoolBuilder<'_>,
) -> Result<VerificationType, String> {
    Ok(match value {
        ValueType::Top => VerificationType::Simple(0),
        ValueType::Int => VerificationType::Simple(1),
        ValueType::Float => VerificationType::Simple(2),
        ValueType::Double => VerificationType::Simple(3),
        ValueType::Long => VerificationType::Simple(4),
        ValueType::Null => VerificationType::Simple(5),
        ValueType::UninitializedThis => VerificationType::Simple(6),
        ValueType::Object(name) => VerificationType::Object(pool.class(name)?),
        ValueType::Uninitialized(offset) => VerificationType::Uninitialized(shifted_u2(
            label,
            map_offset(label, plan, *offset, true)?,
            "an uninitialized offset",
        )?),
    })
}

/// One `StackMapTable` frame with its locals and stack expanded to absolute,
/// delta-free lists.
fn full_frames(
    target: &Target<'_>,
    frames: &[StackMapFrame],
) -> Result<Vec<(usize, Vec<ValueType>, Vec<ValueType>)>, String> {
    let mut locals = initial_local_values(target)?;
    let mut out = Vec::with_capacity(frames.len());
    for frame in frames {
        match frame.frame_type {
            0..=63 | 64..=127 | 247 => {}
            248..=251 => {
                let chop = 251 - frame.frame_type as usize;
                if chop > locals.len() {
                    return Err(format!(
                        "{}: a stack-map chop frame removes more locals than the method declares",
                        target.label()
                    ));
                }
                locals.truncate(locals.len() - chop);
            }
            252..=254 => {
                for verification in &frame.locals {
                    locals.push(value_from_verification(target.pool, verification)?);
                }
            }
            255 => {
                locals = frame
                    .locals
                    .iter()
                    .map(|verification| value_from_verification(target.pool, verification))
                    .collect::<Result<Vec<_>, _>>()?;
            }
            other => {
                return Err(format!(
                    "{}: stack-map frame type {other} is reserved",
                    target.label()
                ));
            }
        }
        let stack = frame
            .stack
            .iter()
            .map(|verification| value_from_verification(target.pool, verification))
            .collect::<Result<Vec<_>, _>>()?;
        out.push((frame.offset, locals.clone(), stack));
    }
    Ok(out)
}

/// The verifier state this module derives at one bytecode offset.
struct SimState {
    locals: Vec<ValueType>,
    stack: Vec<ValueType>,
}

fn pop_stack(stack: &mut Vec<ValueType>, count: usize) -> Result<(), String> {
    for _ in 0..count {
        stack
            .pop()
            .ok_or_else(|| "the operand stack underflows while deriving a frame".to_owned())?;
    }
    Ok(())
}

fn read_local(locals: &[ValueType], slot: usize) -> Result<ValueType, String> {
    if slot > 0
        && matches!(
            locals.get(slot - 1),
            Some(ValueType::Long | ValueType::Double)
        )
    {
        return Err(format!(
            "local slot {slot} is the second half of a wide local"
        ));
    }
    match locals.get(slot) {
        Some(ValueType::Top) | None => Err(format!("local slot {slot} holds no value")),
        Some(value) => Ok(value.clone()),
    }
}

fn write_local(locals: &mut Vec<ValueType>, slot: usize, value: ValueType) {
    // A store over either half of an old wide value leaves the other half
    // unusable, exactly as a `Top` slot.
    if slot > 0
        && matches!(
            locals.get(slot - 1),
            Some(ValueType::Long | ValueType::Double)
        )
    {
        locals[slot - 1] = ValueType::Top;
    }
    if matches!(locals.get(slot), Some(ValueType::Long | ValueType::Double))
        && let Some(next) = locals.get_mut(slot + 1)
    {
        *next = ValueType::Top;
    }
    if locals.len() <= slot {
        locals.resize(slot + 1, ValueType::Top);
    }
    let wide = value.width() == 2;
    locals[slot] = value;
    if wide {
        if locals.len() <= slot + 1 {
            locals.resize(slot + 2, ValueType::Top);
        }
        locals[slot + 1] = ValueType::Top;
    }
}

fn code_u1(code: &[u8], at: usize) -> Result<u8, String> {
    code.get(at)
        .copied()
        .ok_or_else(|| format!("bytecode is truncated at offset {at}"))
}

fn code_u2(code: &[u8], at: usize) -> Result<u16, String> {
    let bytes = code
        .get(at..at + 2)
        .ok_or_else(|| format!("bytecode is truncated at offset {at}"))?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// The top one or two values that span two local/operand slots: a `long` or
/// `double`, or two category-1 values.
fn pop_two_slots(stack: &mut Vec<ValueType>) -> Result<Vec<ValueType>, String> {
    let top = stack
        .pop()
        .ok_or_else(|| "the operand stack underflows while deriving a frame".to_owned())?;
    if top.width() == 2 {
        return Ok(vec![top]);
    }
    let under = stack
        .pop()
        .ok_or_else(|| "the operand stack underflows while deriving a frame".to_owned())?;
    if under.width() != 1 {
        return Err("a category-2 value sits where a category-1 value is required".to_owned());
    }
    Ok(vec![under, top])
}

fn pop_category_one(stack: &mut Vec<ValueType>) -> Result<ValueType, String> {
    let value = stack
        .pop()
        .ok_or_else(|| "the operand stack underflows while deriving a frame".to_owned())?;
    if value.width() != 1 {
        return Err("a category-2 value sits where a category-1 value is required".to_owned());
    }
    Ok(value)
}

fn push_values(stack: &mut Vec<ValueType>, values: Vec<ValueType>) {
    stack.extend(values);
}

/// The component type an `aaload` reads from an array descriptor.
fn array_component(descriptor: &str) -> Result<ValueType, String> {
    let rest = descriptor
        .strip_prefix('[')
        .ok_or_else(|| format!("{descriptor:?} is not an array descriptor"))?;
    if rest.starts_with('[') {
        Ok(ValueType::Object(rest.to_owned()))
    } else {
        ValueType::from_descriptor(rest)
    }
}

/// The type one `ldc`/`ldc_w` constant pushes.
fn constant_type(pool: &[Cp], index: u16) -> Result<ValueType, String> {
    Ok(match pool.get(index as usize) {
        Some(Cp::Integer(_)) => ValueType::Int,
        Some(Cp::Float(_)) => ValueType::Float,
        Some(Cp::String(_)) => ValueType::Object("java/lang/String".to_owned()),
        Some(Cp::Class(_)) => ValueType::Object("java/lang/Class".to_owned()),
        Some(Cp::MethodType) => ValueType::Object("java/lang/invoke/MethodType".to_owned()),
        Some(Cp::MethodHandle { .. }) => {
            ValueType::Object("java/lang/invoke/MethodHandle".to_owned())
        }
        other => {
            return Err(format!(
                "ldc constant {other:?} has no operand-stack type in the bounded derivation"
            ));
        }
    })
}

/// Apply one instruction to the derived state.  `Ok(true)` means control falls
/// through to the next instruction, `Ok(false)` that it transfers elsewhere
/// (or ends the method).
fn simulate_instruction(
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    at: usize,
    state: &mut SimState,
) -> Result<bool, String> {
    let bytes = code.code;
    let opcode = code_u1(bytes, at)?;
    match opcode {
        0x00 => {}
        0x01 => state.stack.push(ValueType::Null),
        0x02..=0x08 => state.stack.push(ValueType::Int),
        0x09..=0x0a => state.stack.push(ValueType::Long),
        0x0b..=0x0d => state.stack.push(ValueType::Float),
        0x0e..=0x0f => state.stack.push(ValueType::Double),
        0x10 => state.stack.push(ValueType::Int),
        0x11 => state.stack.push(ValueType::Int),
        0x12 => {
            let index = code_u1(bytes, at + 1)? as u16;
            state.stack.push(constant_type(target.pool, index)?);
        }
        0x13 => {
            let index = code_u2(bytes, at + 1)?;
            state.stack.push(constant_type(target.pool, index)?);
        }
        0x14 => {
            let index = code_u2(bytes, at + 1)?;
            let value = match target.pool.get(index as usize) {
                Some(Cp::Long(_)) => ValueType::Long,
                Some(Cp::Double(_)) => ValueType::Double,
                other => {
                    return Err(format!("ldc2_w constant {other:?} is not a long or double"));
                }
            };
            state.stack.push(value);
        }
        0x15..=0x19 => {
            let slot = code_u1(bytes, at + 1)? as usize;
            state.stack.push(read_local(&state.locals, slot)?);
        }
        0x1a..=0x1d => {
            let slot = (opcode - 0x1a) as usize;
            state.stack.push(read_local(&state.locals, slot)?);
        }
        0x1e..=0x21 => {
            let slot = (opcode - 0x1e) as usize;
            state.stack.push(read_local(&state.locals, slot)?);
        }
        0x22..=0x25 => {
            let slot = (opcode - 0x22) as usize;
            state.stack.push(read_local(&state.locals, slot)?);
        }
        0x26..=0x29 => {
            let slot = (opcode - 0x26) as usize;
            state.stack.push(read_local(&state.locals, slot)?);
        }
        0x2a..=0x2d => {
            let slot = (opcode - 0x2a) as usize;
            state.stack.push(read_local(&state.locals, slot)?);
        }
        0x2e..=0x35 => {
            pop_stack(&mut state.stack, 1)?; // index
            let array = state
                .stack
                .pop()
                .ok_or_else(|| "an array load has no array on the stack".to_owned())?;
            let loaded = match opcode {
                0x2e | 0x33 | 0x34 | 0x35 => ValueType::Int,
                0x2f => ValueType::Long,
                0x30 => ValueType::Float,
                0x31 => ValueType::Double,
                _ => match array {
                    ValueType::Object(descriptor) => array_component(&descriptor)?,
                    _ => ValueType::Object("java/lang/Object".to_owned()),
                },
            };
            state.stack.push(loaded);
        }
        0x36..=0x3a => {
            let slot = code_u1(bytes, at + 1)? as usize;
            let value = state
                .stack
                .pop()
                .ok_or_else(|| "the operand stack underflows while deriving a frame".to_owned())?;
            write_local(&mut state.locals, slot, value);
        }
        0x3b..=0x3e => {
            let slot = (opcode - 0x3b) as usize;
            let value = state
                .stack
                .pop()
                .ok_or_else(|| "the operand stack underflows while deriving a frame".to_owned())?;
            write_local(&mut state.locals, slot, value);
        }
        0x3f..=0x42 => {
            let slot = (opcode - 0x3f) as usize;
            let value = state
                .stack
                .pop()
                .ok_or_else(|| "the operand stack underflows while deriving a frame".to_owned())?;
            write_local(&mut state.locals, slot, value);
        }
        0x43..=0x46 => {
            let slot = (opcode - 0x43) as usize;
            let value = state
                .stack
                .pop()
                .ok_or_else(|| "the operand stack underflows while deriving a frame".to_owned())?;
            write_local(&mut state.locals, slot, value);
        }
        0x47..=0x4a => {
            let slot = (opcode - 0x47) as usize;
            let value = state
                .stack
                .pop()
                .ok_or_else(|| "the operand stack underflows while deriving a frame".to_owned())?;
            write_local(&mut state.locals, slot, value);
        }
        0x4b..=0x4e => {
            let slot = (opcode - 0x4b) as usize;
            let value = state
                .stack
                .pop()
                .ok_or_else(|| "the operand stack underflows while deriving a frame".to_owned())?;
            write_local(&mut state.locals, slot, value);
        }
        0x4f..=0x56 => pop_stack(&mut state.stack, 3)?,
        0x57 => pop_stack(&mut state.stack, 1)?,
        0x58 => {
            let _ = pop_two_slots(&mut state.stack)?;
        }
        0x59 => {
            let value = pop_category_one(&mut state.stack)?;
            state.stack.push(value.clone());
            state.stack.push(value);
        }
        0x5a => {
            let top = pop_category_one(&mut state.stack)?;
            let under = pop_category_one(&mut state.stack)?;
            state.stack.push(top.clone());
            state.stack.push(under);
            state.stack.push(top);
        }
        0x5b => {
            let top = pop_category_one(&mut state.stack)?;
            let under = pop_two_slots(&mut state.stack)?;
            state.stack.push(top.clone());
            push_values(&mut state.stack, under);
            state.stack.push(top);
        }
        0x5c => {
            let group = pop_two_slots(&mut state.stack)?;
            push_values(&mut state.stack, group.clone());
            push_values(&mut state.stack, group);
        }
        0x5d => {
            let group = pop_two_slots(&mut state.stack)?;
            let under = pop_category_one(&mut state.stack)?;
            push_values(&mut state.stack, group.clone());
            state.stack.push(under);
            push_values(&mut state.stack, group);
        }
        0x5e => {
            let group = pop_two_slots(&mut state.stack)?;
            let under = pop_two_slots(&mut state.stack)?;
            push_values(&mut state.stack, group.clone());
            push_values(&mut state.stack, under);
            push_values(&mut state.stack, group);
        }
        0x5f => {
            let top = pop_category_one(&mut state.stack)?;
            let under = pop_category_one(&mut state.stack)?;
            state.stack.push(top);
            state.stack.push(under);
        }
        0x60..=0x63 => {
            pop_stack(&mut state.stack, 2)?;
            state.stack.push(match opcode {
                0x61 => ValueType::Long,
                0x62 => ValueType::Float,
                0x63 => ValueType::Double,
                _ => ValueType::Int,
            });
        }
        0x64..=0x67 => {
            pop_stack(&mut state.stack, 2)?;
            state.stack.push(match opcode {
                0x65 => ValueType::Long,
                0x66 => ValueType::Float,
                0x67 => ValueType::Double,
                _ => ValueType::Int,
            });
        }
        0x68..=0x6b => {
            pop_stack(&mut state.stack, 2)?;
            state.stack.push(match opcode {
                0x69 => ValueType::Long,
                0x6a => ValueType::Float,
                0x6b => ValueType::Double,
                _ => ValueType::Int,
            });
        }
        0x6c..=0x6f => {
            pop_stack(&mut state.stack, 2)?;
            state.stack.push(match opcode {
                0x6d => ValueType::Long,
                0x6e => ValueType::Float,
                0x6f => ValueType::Double,
                _ => ValueType::Int,
            });
        }
        0x70..=0x73 => {
            pop_stack(&mut state.stack, 2)?;
            state.stack.push(match opcode {
                0x71 => ValueType::Long,
                0x72 => ValueType::Float,
                0x73 => ValueType::Double,
                _ => ValueType::Int,
            });
        }
        0x74..=0x77 => {
            pop_stack(&mut state.stack, 1)?;
            state.stack.push(match opcode {
                0x75 => ValueType::Long,
                0x76 => ValueType::Float,
                0x77 => ValueType::Double,
                _ => ValueType::Int,
            });
        }
        0x78 | 0x7a | 0x7c => {
            pop_stack(&mut state.stack, 2)?;
            state.stack.push(ValueType::Int);
        }
        0x79 | 0x7b | 0x7d => {
            pop_stack(&mut state.stack, 2)?;
            state.stack.push(ValueType::Long);
        }
        0x7e | 0x80 | 0x82 => {
            pop_stack(&mut state.stack, 2)?;
            state.stack.push(ValueType::Int);
        }
        0x7f | 0x81 | 0x83 => {
            pop_stack(&mut state.stack, 2)?;
            state.stack.push(ValueType::Long);
        }
        0x84 => {
            // `iinc` keeps the local's `int` type.
        }
        0x85..=0x93 => {
            let converted = match opcode {
                0x85 => ValueType::Long,
                0x86 => ValueType::Float,
                0x87 => ValueType::Double,
                0x88 | 0x8b | 0x8e | 0x91 | 0x92 | 0x93 => ValueType::Int,
                0x89 | 0x8c | 0x8f => ValueType::Float,
                0x8a | 0x8d | 0x90 => ValueType::Double,
                _ => unreachable!(),
            };
            pop_stack(&mut state.stack, 1)?;
            state.stack.push(converted);
        }
        0x94..=0x98 => {
            pop_stack(&mut state.stack, 2)?;
            state.stack.push(ValueType::Int);
        }
        0x99..=0x9e | 0xc6 | 0xc7 => pop_stack(&mut state.stack, 1)?,
        0x9f..=0xa6 => pop_stack(&mut state.stack, 2)?,
        0xa7 => return Ok(false),
        0xa8 => {
            return Err(
                "the derived state cannot follow a jsr; the bounded weaver does not rewrite subroutines"
                    .to_owned(),
            );
        }
        0xa9 => return Ok(false),
        0xac..=0xb1 => return Ok(false),
        0xb2 | 0xb4 => {
            let index = code_u2(bytes, at + 1)?;
            let descriptor = pool_field_type(target.pool, index)
                .ok_or_else(|| format!("field reference {index} is missing from the pool"))?;
            if opcode == 0xb4 {
                pop_stack(&mut state.stack, 1)?;
            }
            state.stack.push(ValueType::from_descriptor(descriptor)?);
        }
        0xb3 | 0xb5 => {
            let index = code_u2(bytes, at + 1)?;
            if pool_field_type(target.pool, index).is_none() {
                return Err(format!("field reference {index} is missing from the pool"));
            }
            pop_stack(&mut state.stack, 1)?;
            if opcode == 0xb5 {
                pop_stack(&mut state.stack, 1)?;
            }
        }
        0xb6..=0xb9 => {
            let index = code_u2(bytes, at + 1)?;
            let (name, descriptor) = pool_method(target.pool, index)
                .ok_or_else(|| format!("method reference {index} is missing from the pool"))?;
            let arguments = crate::descriptor_arguments(descriptor);
            pop_stack(&mut state.stack, arguments.len())?;
            let receiver = if opcode == 0xb8 {
                None
            } else {
                Some(
                    state
                        .stack
                        .pop()
                        .ok_or_else(|| "a method call has no receiver on the stack".to_owned())?,
                )
            };
            if let Some(receiver) = receiver
                && opcode == 0xb7
                && name == "<init>"
            {
                let uninitialized = receiver.clone();
                let initialized = match receiver {
                    ValueType::Uninitialized(offset) => {
                        let new_at = offset;
                        let class_index = code_u2(bytes, new_at + 1)?;
                        ValueType::Object(
                            pool_class_at(target.pool, class_index)
                                .ok_or_else(|| {
                                    format!("new at offset {new_at} names an unknown class")
                                })?
                                .to_owned(),
                        )
                    }
                    ValueType::UninitializedThis => ValueType::Object(target.class_name.clone()),
                    other => other,
                };
                for value in state.stack.iter_mut().chain(state.locals.iter_mut()) {
                    if *value == uninitialized {
                        *value = initialized.clone();
                    }
                }
            }
            let returned = crate::return_desc(descriptor);
            if returned != "V" {
                state.stack.push(ValueType::from_descriptor(returned)?);
            }
        }
        0xba => {
            let index = code_u2(bytes, at + 1)?;
            let descriptor =
                pool_invoke_dynamic_descriptor(target.pool, index).ok_or_else(|| {
                    format!("invokedynamic reference {index} is missing from the pool")
                })?;
            pop_stack(
                &mut state.stack,
                crate::descriptor_arguments(descriptor).len(),
            )?;
            let returned = crate::return_desc(descriptor);
            if returned != "V" {
                state.stack.push(ValueType::from_descriptor(returned)?);
            }
        }
        0xbb => {
            let index = code_u2(bytes, at + 1)?;
            let name = pool_class_at(target.pool, index)
                .ok_or_else(|| format!("new names class {index}, which is not a class"))?;
            state.stack.push(ValueType::Uninitialized(at));
            let _ = name;
        }
        0xbc => {
            pop_stack(&mut state.stack, 1)?;
            let array = match code_u1(bytes, at + 1)? {
                4 => "[Z",
                5 => "[C",
                6 => "[F",
                7 => "[D",
                8 => "[B",
                9 => "[S",
                10 => "[I",
                11 => "[J",
                other => {
                    return Err(format!(
                        "newarray type {other} is not a primitive array type"
                    ));
                }
            };
            state.stack.push(ValueType::Object(array.to_owned()));
        }
        0xbd => {
            pop_stack(&mut state.stack, 1)?;
            let index = code_u2(bytes, at + 1)?;
            let name = pool_class_at(target.pool, index)
                .ok_or_else(|| format!("anewarray names class {index}, which is not a class"))?;
            state.stack.push(ValueType::Object(format!("[L{name};")));
        }
        0xbe => {
            pop_stack(&mut state.stack, 1)?;
            state.stack.push(ValueType::Int);
        }
        0xbf => {
            pop_stack(&mut state.stack, 1)?;
            return Ok(false);
        }
        0xc0 => {
            pop_stack(&mut state.stack, 1)?;
            let index = code_u2(bytes, at + 1)?;
            let name = pool_class_at(target.pool, index)
                .ok_or_else(|| format!("checkcast names class {index}, which is not a class"))?;
            state.stack.push(ValueType::Object(name.to_owned()));
        }
        0xc1 => {
            pop_stack(&mut state.stack, 1)?;
            state.stack.push(ValueType::Int);
        }
        0xc2 | 0xc3 => pop_stack(&mut state.stack, 1)?,
        0xc4 => {
            let sub = code_u1(bytes, at + 1)?;
            match sub {
                0x15..=0x19 => {
                    let slot = code_u2(bytes, at + 2)? as usize;
                    state.stack.push(read_local(&state.locals, slot)?);
                }
                0x36..=0x3a => {
                    let slot = code_u2(bytes, at + 2)? as usize;
                    let value = state.stack.pop().ok_or_else(|| {
                        "the operand stack underflows while deriving a frame".to_owned()
                    })?;
                    write_local(&mut state.locals, slot, value);
                }
                0x84 => {}
                0xa9 => return Ok(false),
                other => {
                    return Err(format!(
                        "wide prefixes opcode 0x{other:02x}, which the bounded derivation cannot follow"
                    ));
                }
            }
        }
        0xc5 => {
            let dimensions = code_u1(bytes, at + 3)? as usize;
            pop_stack(&mut state.stack, dimensions)?;
            let index = code_u2(bytes, at + 1)?;
            let name = pool_class_at(target.pool, index).ok_or_else(|| {
                format!("multianewarray names class {index}, which is not a class")
            })?;
            state.stack.push(ValueType::Object(name.to_owned()));
        }
        0xc8 => return Ok(false),
        0xc9 => {
            return Err(
                "the derived state cannot follow a jsr_w; the bounded weaver does not rewrite subroutines"
                    .to_owned(),
            );
        }
        other => {
            return Err(format!(
                "the bounded frame derivation does not model opcode 0x{other:02x}"
            ));
        }
    }
    Ok(true)
}

/// Derive the verifier state just before the instruction at `stop` by following
/// the straight-line region from the nearest preceding `StackMapTable` frame
/// (or the method entry), and applying every instruction's effect on the local
/// variables and the operand stack.
///
/// The region between two frames cannot have a second entry - an instruction a
/// branch could reach would carry its own frame - so a straight-line walk is
/// exact.  An instruction the bounded model cannot follow, or control that does
/// not fall through, is a reason rather than a guessed frame.
fn derive_state_at(
    label: &str,
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
    stop: usize,
) -> Result<SimState, String> {
    let frames = match code
        .attributes
        .iter()
        .find_map(|attribute| match attribute {
            CodeAttribute::StackMap { frames, .. } => Some(frames),
            _ => None,
        }) {
        Some(frames) => full_frames(target, frames)?,
        None => Vec::new(),
    };
    let (start, mut state) = match frames.iter().rev().find(|(offset, _, _)| *offset <= stop) {
        Some((offset, locals, stack)) => (
            *offset,
            SimState {
                locals: slots_from_values(locals),
                stack: stack.clone(),
            },
        ),
        None => (
            0,
            SimState {
                locals: slots_from_values(&initial_local_values(target)?),
                stack: Vec::new(),
            },
        ),
    };
    if start == stop {
        return Ok(state);
    }
    let instructions = scan
        .instructions
        .iter()
        .map(|instruction| (instruction.at, instruction))
        .collect::<BTreeMap<_, _>>();
    let mut at = start;
    while at < stop {
        let instruction = instructions.get(&at).ok_or_else(|| {
            format!(
                "{label}: cannot derive the verifier state at {stop}: offset {at} is not an instruction boundary"
            )
        })?;
        if instruction.switch.is_some() {
            pop_stack(&mut state.stack, 1)?;
        } else if !simulate_instruction(target, code, at, &mut state)? {
            return Err(format!(
                "{label}: cannot derive the verifier state at {stop}: control does not fall through from offset {at}"
            ));
        }
        at = instruction.at + instruction.size;
    }
    Ok(state)
}

/// The opcode of the branch that skips the wide jump of an expanded
/// conditional, or `None` when the branch cannot be split this way.
fn inverse_conditional(opcode: u8) -> Option<u8> {
    Some(match opcode {
        0x99 => 0x9a, // ifeq <-> ifne
        0x9a => 0x99,
        0x9b => 0x9c, // iflt <-> ifge
        0x9c => 0x9b,
        0x9d => 0x9e, // ifgt <-> ifle
        0x9e => 0x9d,
        0x9f => 0xa0, // if_icmpeq <-> if_icmpne
        0xa0 => 0x9f,
        0xa1 => 0xa2, // if_icmplt <-> if_icmpge
        0xa2 => 0xa1,
        0xa3 => 0xa4, // if_icmpgt <-> if_icmple
        0xa4 => 0xa3,
        0xa5 => 0xa6, // if_acmpeq <-> if_acmpne
        0xa6 => 0xa5,
        0xc6 => 0xc7, // ifnull <-> ifnonnull
        0xc7 => 0xc6,
        _ => return None,
    })
}

/// The instruction bytes for one callback call, and the stack/local slots they
/// need.
struct CallbackCall {
    bytes: Vec<u8>,
    /// Operand-stack slots the loads push at their peak.
    stack_slots: u16,
    /// Local slots the target's arguments (and receiver) occupy.
    local_slots: u16,
}

/// Append a local-variable instruction naming `slot`.
///
/// A slot above 255 needs the `wide` prefix; the class-file format caps a
/// method's parameter slots well below that, so this only runs for a hand-built
/// descriptor or a method that already declares 255+ locals, but emitting the
/// wide form keeps the insert faithful to the slot it names instead of silently
/// truncating the index.
fn local_opcode(out: &mut Vec<u8>, opcode: u8, slot: u16) {
    if slot > u8::MAX as u16 {
        out.push(0xc4); // wide
        out.push(opcode);
        out.extend_from_slice(&slot.to_be_bytes());
    } else {
        out.push(opcode);
        out.push(slot as u8);
    }
}

/// One local load of a callback argument.
#[derive(Debug, Clone, Copy)]
struct LocalLoad {
    /// The opcode: the compressed one-byte form (`aload_0`) for the receiver,
    /// the indexed form (`iload 1`) for an argument.
    opcode: u8,
    /// The slot an indexed opcode names; `None` for the compressed form.
    slot: Option<u16>,
    /// Local slots the value spans.
    width: u16,
}

impl LocalLoad {
    /// `aload_0`, which supplies an instance callback's receiver.
    fn receiver() -> Self {
        Self {
            opcode: 0x2a,
            slot: None,
            width: 1,
        }
    }

    fn argument(opcode: u8, slot: u16, width: u16) -> Self {
        Self {
            opcode,
            slot: Some(slot),
            width,
        }
    }

    fn emit(&self, out: &mut Vec<u8>) {
        match self.slot {
            Some(slot) => local_opcode(out, self.opcode, slot),
            None => out.push(self.opcode),
        }
    }

    /// The local slot the load reads, and the slots it spans.
    fn span(&self) -> (u16, u16) {
        (self.slot.unwrap_or(0), self.width)
    }
}

/// The local loads a callback's own arguments need, in order: the optional
/// receiver first, then the target's arguments, each with the local slot it
/// occupies and the slots it spans.
fn argument_loads(target: &Target<'_>, takes_receiver: bool) -> Result<Vec<LocalLoad>, String> {
    let arguments = crate::descriptor_arguments(&target.descriptor);
    let mut loads = Vec::with_capacity(arguments.len() + 1);
    let mut slot: u16 = if target.is_static { 0 } else { 1 };
    if takes_receiver {
        loads.push(LocalLoad::receiver());
    }
    for argument in &arguments {
        let (opcode, width) = load_opcode(argument)?;
        loads.push(LocalLoad::argument(opcode, slot, width));
        slot = slot.checked_add(width).ok_or("local slot overflow")?;
    }
    Ok(loads)
}

/// The local slots the target's arguments occupy, receiver included.
fn argument_slots(target: &Target<'_>) -> Result<u16, String> {
    let mut slots: u16 = if target.is_static { 0 } else { 1 };
    for argument in crate::descriptor_arguments(&target.descriptor) {
        let (_, width) = load_opcode(&argument)?;
        slots = slots.checked_add(width).ok_or("local slot overflow")?;
    }
    Ok(slots)
}

/// Emit the loads for one callback and its `invokestatic`.
///
/// `local_slots` is the highest local slot the loads read plus one, so the
/// caller can raise `max_locals` when the class declares fewer; `stack_slots`
/// is the number of operand-stack slots they push at their peak, so the caller
/// can raise `max_stack` conservatively.
fn callback_call(
    target: &Target<'_>,
    callback: &MethodRef,
    takes_receiver: bool,
    pool: &mut PoolBuilder<'_>,
) -> Result<CallbackCall, String> {
    let mut inserted = Vec::new();
    let mut slots = 0u16;
    let mut local_slots = if target.is_static { 0 } else { 1 };
    for load in argument_loads(target, takes_receiver)? {
        load.emit(&mut inserted);
        let (slot, width) = load.span();
        local_slots = local_slots.max(slot.checked_add(width).ok_or("local slot overflow")?);
        slots = slots.checked_add(width).ok_or("argument slot overflow")?;
    }
    let callback_index = pool.method_ref(callback)?;
    inserted.push(0xb8); // invokestatic
    inserted.extend_from_slice(&callback_index.to_be_bytes());
    Ok(CallbackCall {
        bytes: inserted,
        stack_slots: slots,
        local_slots,
    })
}

/// Emit the loads for one non-cancellable callback whose last argument is a
/// plain `CallbackInfo`.
///
/// The object is constructed after the arguments, because it has to be the
/// topmost value at the call: the stack ends as `[..., receiver?, args..., ci]`
/// and `invokestatic` consumes it.  Nothing keeps a reference to the object
/// afterwards, so it lives only on the operand stack: no fresh local, no
/// `max_locals` growth and no verifier frame changes, which is what lets the
/// same insert sit in front of a `return` without disturbing its result.
fn plain_callback_info_call(
    target: &Target<'_>,
    callback: &MethodRef,
    takes_receiver: bool,
    pool: &mut PoolBuilder<'_>,
) -> Result<CallbackCall, String> {
    let mut inserted = Vec::new();
    let mut slots = 0u16;
    let mut local_slots = if target.is_static { 0 } else { 1 };
    for load in argument_loads(target, takes_receiver)? {
        load.emit(&mut inserted);
        let (slot, width) = load.span();
        local_slots = local_slots.max(slot.checked_add(width).ok_or("local slot overflow")?);
        slots = slots.checked_add(width).ok_or("argument slot overflow")?;
    }
    let info = pool.class(CALLBACK_INFO_CLASS)?;
    inserted.push(0xbb); // new CallbackInfo
    inserted.extend_from_slice(&info.to_be_bytes());
    inserted.push(0x59); // dup
    let init = pool.method_ref(&MethodRef {
        owner: CALLBACK_INFO_CLASS.to_owned(),
        name: "<init>".to_owned(),
        descriptor: "()V".to_owned(),
    })?;
    inserted.push(0xb7); // invokespecial CallbackInfo.<init>()V
    inserted.extend_from_slice(&init.to_be_bytes());
    let callback_index = pool.method_ref(callback)?;
    inserted.push(0xb8); // invokestatic callback
    inserted.extend_from_slice(&callback_index.to_be_bytes());
    Ok(CallbackCall {
        bytes: inserted,
        // The arguments and the constructed object.
        stack_slots: slots.saturating_add(1),
        local_slots,
    })
}

/// The wrapper class, `valueOf` descriptor and unboxing method that move one
/// primitive return descriptor through a `CallbackInfoReturnable`.
fn primitive_wrapper(descriptor: &str) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match descriptor {
        "Z" => (
            "java/lang/Boolean",
            "(Z)Ljava/lang/Boolean;",
            "booleanValue",
        ),
        "B" => ("java/lang/Byte", "(B)Ljava/lang/Byte;", "byteValue"),
        "C" => (
            "java/lang/Character",
            "(C)Ljava/lang/Character;",
            "charValue",
        ),
        "S" => ("java/lang/Short", "(S)Ljava/lang/Short;", "shortValue"),
        "I" => ("java/lang/Integer", "(I)Ljava/lang/Integer;", "intValue"),
        "J" => ("java/lang/Long", "(J)Ljava/lang/Long;", "longValue"),
        "F" => ("java/lang/Float", "(F)Ljava/lang/Float;", "floatValue"),
        "D" => ("java/lang/Double", "(D)Ljava/lang/Double;", "doubleValue"),
        _ => return None,
    })
}

/// The `CONSTANT_Class` name an `areturn` type's `checkcast` names: the class
/// name inside `L...;`, or the array descriptor itself.
fn reference_class_name(descriptor: &str) -> Result<&str, String> {
    if let Some(name) = descriptor
        .strip_prefix('L')
        .and_then(|rest| rest.strip_suffix(';'))
        && !name.is_empty()
        && !name.contains(';')
    {
        return Ok(name);
    }
    if descriptor.starts_with('[') {
        return Ok(descriptor);
    }
    Err(format!(
        "return descriptor {descriptor:?} is neither a primitive nor a reference type"
    ))
}

/// Emit one non-void `RETURN`/`TAIL` insert.
///
/// The target's result is on the operand stack when the insert runs.  The insert
/// boxes it into the fresh local `slot`, builds a `CallbackInfoReturnable`
/// around it, calls the callback with the woven method's arguments and that
/// object, then replaces the result with `getReturnValue()` in the target's own
/// shape.  No branch is needed: the object starts with the target's value, so
/// "no override" and "override" are the same read.
///
/// `slot` holds the boxed value and then the object itself - the value is dead
/// once `<init>` ran - so one fresh local is enough.  It sits at the callback's
/// own argument position, which is why the object is built before the arguments
/// are pushed: the arguments consume the topmost object and the second `aload`
/// keeps the woven method's own reference for `getReturnValue`.
fn returnable_call(
    target: &Target<'_>,
    callback: &MethodRef,
    takes_receiver: bool,
    slot: u16,
    pool: &mut PoolBuilder<'_>,
) -> Result<CallbackCall, String> {
    let returns = crate::return_desc(&target.descriptor);
    let wrapper = primitive_wrapper(returns);
    let mut bytes = Vec::new();
    if let Some((wrapper, box_descriptor, _)) = wrapper {
        let value_of = pool.method_ref(&MethodRef {
            owner: wrapper.to_owned(),
            name: "valueOf".to_owned(),
            descriptor: box_descriptor.to_owned(),
        })?;
        bytes.push(0xb8); // invokestatic <wrapper>.valueOf
        bytes.extend_from_slice(&value_of.to_be_bytes());
    }
    local_opcode(&mut bytes, 0x3a, slot); // astore <slot>
    let returnable = pool.class(CALLBACK_INFO_RETURNABLE_CLASS)?;
    bytes.push(0xbb); // new CallbackInfoReturnable
    bytes.extend_from_slice(&returnable.to_be_bytes());
    bytes.push(0x59); // dup
    local_opcode(&mut bytes, 0x19, slot); // aload <slot>
    let init = pool.method_ref(&MethodRef {
        owner: CALLBACK_INFO_RETURNABLE_CLASS.to_owned(),
        name: "<init>".to_owned(),
        descriptor: "(Ljava/lang/Object;)V".to_owned(),
    })?;
    bytes.push(0xb7); // invokespecial CallbackInfoReturnable.<init>(Object)
    bytes.extend_from_slice(&init.to_be_bytes());
    local_opcode(&mut bytes, 0x3a, slot); // astore <slot>: the value is dead
    let mut slots = 0u16;
    let mut local_slots = slot
        .checked_add(1)
        .ok_or("the fresh local does not fit the class-file local-slot limit")?;
    for load in argument_loads(target, takes_receiver)? {
        load.emit(&mut bytes);
        let (index, width) = load.span();
        local_slots = local_slots.max(index.saturating_add(width));
        slots = slots.saturating_add(width);
    }
    local_opcode(&mut bytes, 0x19, slot); // aload <slot>
    let callback_index = pool.method_ref(callback)?;
    bytes.push(0xb8); // invokestatic callback
    bytes.extend_from_slice(&callback_index.to_be_bytes());
    local_opcode(&mut bytes, 0x19, slot); // aload <slot>
    let get_return = pool.method_ref(&MethodRef {
        owner: CALLBACK_INFO_RETURNABLE_CLASS.to_owned(),
        name: "getReturnValue".to_owned(),
        descriptor: "()Ljava/lang/Object;".to_owned(),
    })?;
    bytes.push(0xb6); // invokevirtual getReturnValue
    bytes.extend_from_slice(&get_return.to_be_bytes());
    match wrapper {
        Some((wrapper, _, unbox)) => {
            let cast = pool.class(wrapper)?;
            bytes.push(0xc0); // checkcast <wrapper>
            bytes.extend_from_slice(&cast.to_be_bytes());
            let unbox = pool.method_ref(&MethodRef {
                owner: wrapper.to_owned(),
                name: unbox.to_owned(),
                descriptor: format!("(){returns}"),
            })?;
            bytes.push(0xb6); // invokevirtual <wrapper>.<unbox>
            bytes.extend_from_slice(&unbox.to_be_bytes());
        }
        None => {
            let cast = pool.class(reference_class_name(returns)?)?;
            bytes.push(0xc0); // checkcast <return type>
            bytes.extend_from_slice(&cast.to_be_bytes());
        }
    }
    Ok(CallbackCall {
        bytes,
        // `new` + `dup` + the boxed value sit above the callback's arguments.
        stack_slots: slots.saturating_add(3),
        local_slots,
    })
}

/// Resolve a bounded ordinal against an ordered site list.
///
/// `Some(n)` is 0-based, and a negative ordinal counts from the end (`-1` is
/// the last); anything outside the list is a reason that names the count.
fn ordinal_index(label: &str, anchor: &str, count: usize, ordinal: i32) -> Result<usize, String> {
    let index = i64::from(ordinal);
    let index = if index < 0 {
        (count as i64)
            .checked_add(index)
            .ok_or_else(|| ordinal_out_of_range(label, anchor, count, ordinal))?
    } else {
        index
    };
    if index < 0 || index >= count as i64 {
        return Err(ordinal_out_of_range(label, anchor, count, ordinal));
    }
    Ok(index as usize)
}

fn ordinal_out_of_range(label: &str, anchor: &str, count: usize, ordinal: i32) -> String {
    format!(
        "{label}: {anchor} weaving ordinal {ordinal} is outside the {count} matching site(s); an ordinal is 0-based and a negative one counts from the end (-1 is the last)"
    )
}

/// Every call instruction offset one `owner.name(descriptor)` selector names.
fn invoke_selector_offsets(
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
    owner: &str,
    name: &str,
    descriptor: &str,
) -> Result<Vec<usize>, String> {
    let mut offsets = Vec::new();
    for instruction in &scan.instructions {
        let opcode = code_u1(code.code, instruction.at)?;
        if !matches!(opcode, 0xb6..=0xb9) {
            continue;
        }
        let index = code_u2(code.code, instruction.at + 1)?;
        let Some((call_owner, call_name, call_descriptor)) = invoke_method_ref(target.pool, index)
        else {
            continue;
        };
        if call_owner == owner
            && call_name == name
            && (descriptor.is_empty() || call_descriptor == descriptor)
        {
            offsets.push(instruction.at);
        }
    }
    Ok(offsets)
}

/// Resolve one return selector's ordinal against the method's return list.
fn return_selector_offset(
    label: &str,
    scan: &CodeScan,
    selector: &SiteSelector,
) -> Result<usize, String> {
    let Some(first) = scan.returns.first() else {
        return Err(format!(
            "{label}: the {} slice boundary needs a return instruction, but the method has none",
            selector.describe()
        ));
    };
    match selector {
        SiteSelector::Return => Ok(*first),
        SiteSelector::Tail => Ok(*scan.returns.last().unwrap_or(first)),
        SiteSelector::ReturnOrdinal { ordinal } | SiteSelector::TailOrdinal { ordinal } => {
            let index = ordinal_index(label, selector.anchor(), scan.returns.len(), *ordinal)?;
            Ok(scan.returns[index])
        }
        _ => unreachable!("return_selector_offset is only called with a return selector"),
    }
}

/// Every instruction offset one slice boundary names, with its ordinal applied.
fn selector_offsets(
    label: &str,
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
    selector: &SiteSelector,
) -> Result<Vec<usize>, String> {
    Ok(match selector {
        SiteSelector::Head => {
            if scan.instructions.is_empty() {
                return Err(format!(
                    "{label}: the method has no instruction for a HEAD boundary"
                ));
            }
            vec![0]
        }
        SiteSelector::Return
        | SiteSelector::Tail
        | SiteSelector::ReturnOrdinal { .. }
        | SiteSelector::TailOrdinal { .. } => {
            vec![return_selector_offset(label, scan, selector)?]
        }
        SiteSelector::Invoke {
            owner,
            name,
            descriptor,
        } => {
            let offsets = invoke_selector_offsets(target, code, scan, owner, name, descriptor)?;
            if offsets.is_empty() {
                return Err(format!(
                    "{label}: the {} slice boundary matches no instruction",
                    selector.describe()
                ));
            }
            offsets
        }
        SiteSelector::InvokeOrdinal {
            owner,
            name,
            descriptor,
            ordinal,
        } => {
            let offsets = invoke_selector_offsets(target, code, scan, owner, name, descriptor)?;
            if offsets.is_empty() {
                return Err(format!(
                    "{label}: the {} slice boundary matches no instruction",
                    selector.describe()
                ));
            }
            let index = ordinal_index(label, selector.anchor(), offsets.len(), *ordinal)?;
            vec![offsets[index]]
        }
    })
}

/// Resolve one slice to the half-open instruction range `[from, to)`.
fn resolve_slice(
    label: &str,
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
    slice: &SliceBounds,
) -> Result<(usize, usize), String> {
    let boundary = |selector: &SiteSelector, which: &str| -> Result<usize, String> {
        let offsets = selector_offsets(label, target, code, scan, selector)?;
        match offsets.as_slice() {
            [at] => Ok(*at),
            many => Err(format!(
                "{label}: the slice {which} boundary {} matches {} instructions, so it does not name one; a slice boundary has to select exactly one instruction",
                selector.describe(),
                many.len()
            )),
        }
    };
    let from = boundary(&slice.from, "from")?;
    let to = boundary(&slice.to, "to")?;
    if from > to {
        return Err(format!(
            "{label}: the slice from {} ({from}) to {} ({to}) ends before it starts",
            slice.from.describe(),
            slice.to.describe()
        ));
    }
    if from == to {
        return Err(format!(
            "{label}: the slice from {} to {} contains no instruction",
            slice.from.describe(),
            slice.to.describe()
        ));
    }
    Ok((from, to))
}

/// One instruction an extended anchor selected, with the facts the callback
/// shape needs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AnchorHitKind {
    Field {
        access: FieldAccess,
        owner: String,
        name: String,
        descriptor: String,
    },
    New {
        owner: String,
    },
    Jump {
        opcode: u8,
    },
    Invoke {
        owner: String,
        name: String,
        descriptor: String,
        /// Whether the call pushes a receiver below its arguments.
        receiver: bool,
        /// `invokespecial`, which a redirect cannot replace because the
        /// receiver may be an uninitialized object.
        special: bool,
    },
}

#[derive(Debug, Clone)]
struct AnchorHit {
    at: usize,
    kind: AnchorHitKind,
}

/// The owner, name and descriptor one `Fieldref` names.
fn field_ref(pool: &[Cp], index: u16) -> Option<(&str, &str, &str)> {
    match pool.get(index as usize) {
        Some(Cp::FieldRef { class, nt }) => {
            let owner = pool_class_at(pool, *class)?;
            let (name, descriptor) = pool_name_type_at(pool, *nt)?;
            Some((owner, name, descriptor))
        }
        _ => None,
    }
}

/// Every field, construction and jump instruction in the method, in file
/// order.
fn scan_anchor_hits(
    label: &str,
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
) -> Result<Vec<AnchorHit>, String> {
    let mut hits = Vec::new();
    for instruction in &scan.instructions {
        let opcode = code_u1(code.code, instruction.at)?;
        let kind = match opcode {
            0xb2..=0xb5 => {
                let index = code_u2(code.code, instruction.at + 1)?;
                let Some((owner, name, descriptor)) = field_ref(target.pool, index) else {
                    continue;
                };
                AnchorHitKind::Field {
                    access: FieldAccess::of_opcode(opcode)
                        .expect("the field opcode range maps to a FieldAccess"),
                    owner: owner.to_owned(),
                    name: name.to_owned(),
                    descriptor: descriptor.to_owned(),
                }
            }
            0xbb => {
                let index = code_u2(code.code, instruction.at + 1)?;
                let Some(owner) = pool_class_at(target.pool, index) else {
                    continue;
                };
                AnchorHitKind::New {
                    owner: owner.to_owned(),
                }
            }
            0x99..=0xa7 | 0xc6 | 0xc7 => AnchorHitKind::Jump { opcode },
            0xb6..=0xb9 => {
                let index = code_u2(code.code, instruction.at + 1)?;
                let Some((owner, name, descriptor)) = invoke_method_ref(target.pool, index) else {
                    continue;
                };
                AnchorHitKind::Invoke {
                    owner: owner.to_owned(),
                    name: name.to_owned(),
                    descriptor: descriptor.to_owned(),
                    receiver: opcode != 0xb8,
                    special: opcode == 0xb7,
                }
            }
            _ => continue,
        };
        hits.push(AnchorHit {
            at: instruction.at,
            kind,
        });
    }
    let _ = label;
    Ok(hits)
}

fn anchor_hit_matches(hit: &AnchorHitKind, site: &AnchorSite) -> bool {
    match (hit, site) {
        (
            AnchorHitKind::Field {
                access,
                owner,
                name,
                descriptor,
            },
            AnchorSite::Field(expected),
        ) => expected.matches(*access, owner, name, descriptor),
        (AnchorHitKind::New { owner }, AnchorSite::New(expected)) => owner == &expected.owner,
        (
            AnchorHitKind::Jump { opcode },
            AnchorSite::Jump(JumpSite {
                opcode: expected, ..
            }),
        ) => expected.is_none_or(|expected| expected == *opcode),
        (
            AnchorHitKind::Invoke {
                owner,
                name,
                descriptor,
                ..
            },
            AnchorSite::Invoke(selector),
        ) => {
            owner == &selector.owner
                && name == &selector.name
                && (selector.descriptor.is_empty() || descriptor == &selector.descriptor)
        }
        _ => false,
    }
}

/// Resolve the sites an [`AnchorRequest`] names: every matching instruction,
/// narrowed by the slice, then the ordinal (0-based, negative from the end),
/// then moved by the shift.
fn select_anchor_hits(
    label: &str,
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
    request: &AnchorRequest,
) -> Result<Vec<AnchorHit>, String> {
    let anchor = request.site.label();
    let mut selected = scan_anchor_hits(label, target, code, scan)?
        .into_iter()
        .filter(|hit| anchor_hit_matches(&hit.kind, &request.site))
        .collect::<Vec<_>>();
    if let AnchorSite::Jump(JumpSite {
        offset: Some(offset),
        ..
    }) = &request.site
    {
        selected.retain(|hit| hit.at == *offset);
    }
    if selected.is_empty() {
        return Err(format!("{label}: {anchor} matches no instruction"));
    }
    if let Some(slice) = &request.slice {
        let (from, to) = resolve_slice(label, target, code, scan, slice)?;
        selected.retain(|hit| hit.at >= from && hit.at < to);
        if selected.is_empty() {
            return Err(format!(
                "{label}: {anchor} has no matching instruction inside the slice [{from}, {to})"
            ));
        }
    }
    if let Some(ordinal) = request.ordinal {
        let index = ordinal_index(label, anchor.as_str(), selected.len(), ordinal)?;
        selected = vec![selected[index].clone()];
    }
    if let Some(shift) = request.shift {
        if request.redirect {
            return Err(format!(
                "{label}: a @Redirect at {anchor} cannot be shifted; its replacement bytes have to stay on the instruction they replace"
            ));
        }
        let delta = shift.distance(label)?;
        if delta != 0 {
            let mut moved = Vec::with_capacity(selected.len());
            for hit in selected {
                let at = shifted_offset(label, code, scan, hit.at, delta)?;
                moved.push(AnchorHit { at, kind: hit.kind });
            }
            selected = moved;
        }
    }
    selected.sort_by_key(|hit| hit.at);
    for pair in selected.windows(2) {
        if pair[0].at == pair[1].at {
            return Err(format!(
                "{label}: two {anchor} sites converge on offset {} after the shift",
                pair[0].at
            ));
        }
    }
    Ok(selected)
}

/// A local a bytecode insert writes and every later stack-map frame has to
/// declare, because a frame's locals list is exact.
#[derive(Debug, Clone)]
struct FrameLocalPatch {
    /// Original offset of the insert that writes the local.
    after: usize,
    slot: u16,
    value: ValueType,
}

/// One bytecode insert and its resource demands.
struct SiteInsert {
    bytes: Vec<u8>,
    /// Operand-stack slots the insert pushes at its peak.
    stack_extra: u16,
    /// The highest local slot the insert reads or writes, plus one.
    local_slots: u16,
    /// Fresh locals the insert leaves live until something overwrites them.
    frame_locals: Vec<(u16, ValueType)>,
    entry: TargetEntry,
    frames: FrameSites,
    /// Replace the original instruction with `nop`s of the same length, so a
    /// redirect's insert is the only thing that performs the access.
    replace_instruction: bool,
}

/// Place one insert and encode the class.
fn insert_once(
    class: &ClassFile,
    target_ref: &MethodRef,
    at: usize,
    insert: SiteInsert,
    pool: &mut PoolBuilder<'_>,
) -> Result<Vec<u8>, String> {
    let target = prepare_target(class, target_ref)?;
    let label = target.label();
    let code = target.code()?;
    let scan = scan_code(&label, code.code)?;
    let max_stack = u32::from(code.max_stack)
        .checked_add(u32::from(insert.stack_extra))
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| format!("{label}: woven max_stack overflows u16"))?;
    let max_locals = code.max_locals.max(insert.local_slots);
    let replace = if insert.replace_instruction {
        let instruction = scan
            .instructions
            .iter()
            .find(|instruction| instruction.at == at)
            .ok_or_else(|| {
                format!("{label}: the redirect site at {at} is not an instruction boundary")
            })?;
        Some((at, instruction.size))
    } else {
        None
    };
    let frame_locals = insert
        .frame_locals
        .iter()
        .map(|(slot, value)| FrameLocalPatch {
            after: at,
            slot: *slot,
            value: value.clone(),
        })
        .collect::<Vec<_>>();
    let patch = relocate_patched(
        &label,
        &target,
        code,
        &scan,
        &[(at, insert.bytes.len())],
        &insert.bytes,
        max_stack,
        max_locals,
        insert.entry,
        insert.frames,
        &[],
        &frame_locals,
        replace,
        pool,
    )?;
    emit(
        &target.layout,
        Some(Patch {
            method: target.method_index,
            attribute: target.attribute_index,
            code: patch,
        }),
        &pool.added,
    )
}

/// Apply one [`AnchorRequest`]'s inserts right to left, re-parsing between
/// edits so every offset is the current one.
///
/// Earlier inserts do not move later offsets, so walking the selected sites in
/// reverse keeps each `i`-th selection stable: the callback call the insert
/// adds could otherwise become another match for a broad selector.
fn apply_anchor_inserts<F>(
    class: &ClassFile,
    target_ref: &MethodRef,
    request: &AnchorRequest,
    mut build: F,
) -> Result<Vec<u8>, String>
where
    F: FnMut(
        &ClassFile,
        &Target<'_>,
        &AnchorHit,
        &mut PoolBuilder<'_>,
    ) -> Result<SiteInsert, String>,
{
    let target = prepare_target(class, target_ref)?;
    let label = target.label();
    let code = target.code()?;
    let scan = scan_code(&label, code.code)?;
    let expected = select_anchor_hits(&label, &target, code, &scan, request)?;
    let mut current = class.write()?;
    for index in (0..expected.len()).rev() {
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        let target = prepare_target(&next, target_ref)?;
        let label = target.label();
        let code = target.code()?;
        let scan = scan_code(&label, code.code)?;
        let selected = select_anchor_hits(&label, &target, code, &scan, request)?;
        if selected.len() != expected.len() {
            return Err(format!(
                "{label}: the anchor selection changed from {} to {} site(s) while weaving; the bounded weaver refuses to guess which site is which",
                expected.len(),
                selected.len()
            ));
        }
        let hit = &selected[index];
        if hit.kind != expected[index].kind {
            return Err(format!(
                "{label}: the anchor site at offset {} changed identity while weaving",
                expected[index].at
            ));
        }
        let mut pool = PoolBuilder::new(&next.pool);
        let insert = build(&next, &target, hit, &mut pool)?;
        current = insert_once(&next, target_ref, hit.at, insert, &mut pool)?;
    }
    Ok(current)
}

/// Validate one callback descriptor against a base argument list.
///
/// `base` is the argument list the callback must consume, in order.  An
/// instance target additionally accepts the target instance as an extra first
/// parameter.  With `allow_info`, a trailing plain `CallbackInfo` is accepted
/// and the caller constructs it.
fn callback_shape_for(
    target: &Target<'_>,
    base: &[String],
    callback_descriptor: &str,
    anchor: &str,
    allow_info: bool,
) -> Result<CallbackShape, String> {
    if !crate::valid_method_descriptor(callback_descriptor) {
        return Err(format!(
            "callback descriptor {callback_descriptor:?} is not a method descriptor"
        ));
    }
    let arguments = crate::descriptor_arguments(callback_descriptor);
    let (core, callback_info) =
        if allow_info && arguments.last().map(String::as_str) == Some(CALLBACK_INFO_DESC) {
            (&arguments[..arguments.len() - 1], true)
        } else {
            (arguments.as_slice(), false)
        };
    let mut with_receiver = Vec::with_capacity(base.len() + 1);
    if !target.is_static {
        with_receiver.push(format!("L{};", target.class_name));
    }
    with_receiver.extend_from_slice(base);
    if core == base {
        return Ok(CallbackShape {
            takes_receiver: false,
            callback_info,
        });
    }
    if core == with_receiver.as_slice() {
        return Ok(CallbackShape {
            takes_receiver: !target.is_static,
            callback_info,
        });
    }
    let detail = if arguments
        .last()
        .is_some_and(|argument| argument == CALLBACK_INFO_RETURNABLE_DESC)
    {
        " (a CallbackInfoReturnable belongs only on a non-void RETURN/TAIL insert)"
    } else {
        ""
    };
    Err(format!(
        "{anchor} callback {callback_descriptor} does not consume the expected arguments {base:?}{detail}"
    ))
}

/// Push the callback's receiver (when it declares one) and the target's
/// arguments, returning the operand-stack slots they occupy.
fn emit_argument_loads(
    target: &Target<'_>,
    takes_receiver: bool,
    out: &mut Vec<u8>,
) -> Result<u16, String> {
    let mut slots = 0u16;
    for load in argument_loads(target, takes_receiver)? {
        load.emit(out);
        slots = slots
            .checked_add(load.span().1)
            .ok_or("argument slot overflow")?;
    }
    Ok(slots)
}

/// Construct one plain `CallbackInfo` on the operand stack.
fn emit_new_callback_info(out: &mut Vec<u8>, pool: &mut PoolBuilder<'_>) -> Result<(), String> {
    let class = pool.class(CALLBACK_INFO_CLASS)?;
    out.push(0xbb); // new
    out.extend_from_slice(&class.to_be_bytes());
    out.push(0x59); // dup
    let init = pool.method_ref(&MethodRef {
        owner: CALLBACK_INFO_CLASS.to_owned(),
        name: "<init>".to_owned(),
        descriptor: "()V".to_owned(),
    })?;
    out.push(0xb7); // invokespecial
    out.extend_from_slice(&init.to_be_bytes());
    Ok(())
}

/// Emit the callback's arguments, an optional value read from a fresh local,
/// the optional `CallbackInfo`, and the `invokestatic`.
fn emit_callback(
    target: &Target<'_>,
    callback: &MethodRef,
    shape: CallbackShape,
    extra: Option<(u16, &str)>,
    pool: &mut PoolBuilder<'_>,
) -> Result<(Vec<u8>, u16, u16), String> {
    let mut out = Vec::new();
    let mut slots = emit_argument_loads(target, shape.takes_receiver, &mut out)?;
    let mut local_slots = argument_slots(target)?;
    if let Some((slot, descriptor)) = extra {
        let (opcode, width) = load_opcode(descriptor)?;
        local_opcode(&mut out, opcode, slot);
        slots = slots.checked_add(width).ok_or("argument slot overflow")?;
        local_slots = local_slots.max(slot.checked_add(width).ok_or("local slot overflow")?);
    }
    if shape.callback_info {
        emit_new_callback_info(&mut out, pool)?;
        slots = slots.checked_add(1).ok_or("argument slot overflow")?;
    }
    let index = pool.method_ref(callback)?;
    out.push(0xb8); // invokestatic
    out.extend_from_slice(&index.to_be_bytes());
    Ok((out, slots, local_slots))
}

/// The `xstore` opcode and width for one value descriptor.
fn store_opcode(descriptor: &str) -> Result<(u8, u16), String> {
    Ok(match descriptor.as_bytes().first().copied() {
        Some(b'I' | b'Z' | b'B' | b'C' | b'S') => (0x36, 1),
        Some(b'J') => (0x37, 2),
        Some(b'F') => (0x38, 1),
        Some(b'D') => (0x39, 2),
        Some(b'L' | b'[') => (0x3a, 1),
        Some(b'V') | None => {
            return Err(format!("descriptor {descriptor:?} is not a value type"));
        }
        Some(other) => {
            return Err(format!(
                "descriptor {descriptor:?} has unknown first byte {}",
                other as char
            ));
        }
    })
}

/// The `xreturn` opcode for one method return descriptor.
fn return_opcode(descriptor: &str) -> Result<u8, String> {
    Ok(match descriptor {
        "V" => 0xb1,
        "J" => 0xad,
        "F" => 0xae,
        "D" => 0xaf,
        "I" | "Z" | "B" | "C" | "S" => 0xac,
        descriptor if descriptor.starts_with('L') || descriptor.starts_with('[') => 0xb0,
        other => return Err(format!("descriptor {other:?} is not a return type")),
    })
}

/// Apply one bounded `@Inject` at a FIELD, NEW or JUMP site.
///
/// A field write whose callback declares the field value gets it as the
/// callback's last argument: the insert stores the value in a fresh local,
/// pushes it after the target arguments, and reloads it for the write.  A
/// callback that does not declare it runs with the target's arguments only,
/// above the field operands the write already pushed.
fn weave_anchor_inject(
    class: &ClassFile,
    target_ref: &MethodRef,
    callback: &MethodRef,
    request: &AnchorRequest,
) -> Result<Vec<u8>, String> {
    if request.cancellable {
        return Err(format!(
            "{}.{}: a cancellable {} @Inject is not a bounded anchor; cancellation belongs to HEAD/RETURN/INVOKE",
            target_ref.owner,
            target_ref.name,
            request.site.label()
        ));
    }
    apply_anchor_inserts(class, target_ref, request, |_class, target, hit, pool| {
        let target_arguments = crate::descriptor_arguments(&target.descriptor);
        let plain = callback_shape_for(
            target,
            &target_arguments,
            &callback.descriptor,
            &format!("@Inject at {}", request.site.label()),
            true,
        );
        let mut valued = None;
        if let AnchorHitKind::Field {
            access, descriptor, ..
        } = &hit.kind
        {
            if access.is_put() {
                let mut base = target_arguments.clone();
                base.push(descriptor.clone());
                if let Ok(shape) = callback_shape_for(
                    target,
                    &base,
                    &callback.descriptor,
                    &format!("@Inject at {}", request.site.label()),
                    true,
                ) {
                    valued = Some((shape, descriptor.clone()));
                }
            }
        }
        let (shape, value_descriptor) = match valued {
            Some((shape, descriptor)) => (shape, Some(descriptor)),
            None => (
                plain.map_err(|error| format!("{}.{}: {error}", target.class_name, target.name))?,
                None,
            ),
        };
        if shape.takes_receiver && target.name == "<init>" {
            return Err(format!(
                "{}: a {} @Inject with the target receiver would pass `this` before the constructor has called super(); the callback must take the target's arguments only",
                target.label(),
                request.site.label()
            ));
        }
        let mut bytes = Vec::new();
        let mut frame_locals = Vec::new();
        let mut local_slots = argument_slots(target)?;
        let stack_extra;
        if let Some(descriptor) = &value_descriptor {
            let (opcode, width) = store_opcode(descriptor)?;
            let slot = target.code()?.max_locals;
            local_opcode(&mut bytes, opcode, slot);
            let (call, slots, _locals) =
                emit_callback(target, callback, shape, Some((slot, descriptor)), pool)?;
            bytes.extend(call);
            let (load, load_width) = load_opcode(descriptor)?;
            local_opcode(&mut bytes, load, slot);
            stack_extra = slots
                .checked_add(load_width)
                .and_then(|value| value.checked_add(1))
                .ok_or("argument slot overflow")?;
            local_slots = local_slots.max(slot.checked_add(width).ok_or("local slot overflow")?);
            frame_locals.push((slot, ValueType::from_descriptor(descriptor)?));
        } else {
            let (call, slots, locals) = emit_callback(target, callback, shape, None, pool)?;
            bytes.extend(call);
            stack_extra = slots.checked_add(1).ok_or("argument slot overflow")?;
            local_slots = local_slots.max(locals);
        }
        let frames = if frame_locals.is_empty() {
            FrameSites::Both
        } else {
            FrameSites::InsertOnly
        };
        Ok(SiteInsert {
            bytes,
            stack_extra,
            local_slots,
            frame_locals,
            entry: TargetEntry::Enter,
            frames,
            replace_instruction: false,
        })
    })
}

/// Apply one bounded `@Redirect` at a FIELD or non-special INVOKE site.
///
/// A read redirect removes the receiver the original access would have
/// consumed, then calls the callback, which returns the field's value.  A write
/// redirect stores the written value in a fresh local, removes the receiver,
/// and calls the callback with the target arguments followed by the value; the
/// callback's `void` return means the original write does not happen.  An
/// INVOKE redirect saves the call's arguments, pops the receiver, calls the
/// callback with the target's arguments, and leaves the original call as
/// `nop`s.  The reads/writes and calls are nop-ed out because an insert cannot
/// remove the bytes that performed them.
fn weave_anchor_redirect(
    class: &ClassFile,
    target_ref: &MethodRef,
    callback: &MethodRef,
    request: &AnchorRequest,
) -> Result<Vec<u8>, String> {
    if request.cancellable {
        return Err(format!(
            "{}.{}: a @Redirect is not cancellable",
            target_ref.owner, target_ref.name
        ));
    }
    apply_anchor_inserts(class, target_ref, request, |_class, target, hit, pool| {
        if let AnchorHitKind::Invoke {
            descriptor,
            receiver,
            special,
            ..
        } = &hit.kind
        {
            return redirect_invoke_insert(target, callback, descriptor, *receiver, *special, pool);
        }
        let AnchorHitKind::Field {
            access, descriptor, ..
        } = &hit.kind
        else {
            return Err(format!(
                "{}: @Redirect at {} is not a bounded anchor; redirects replace FIELD reads/writes, INVOKE calls and a HEAD body",
                target.label(),
                request.site.label()
            ));
        };
        let target_arguments = crate::descriptor_arguments(&target.descriptor);
        let mut bytes = Vec::new();
        let mut frame_locals = Vec::new();
        let mut local_slots = argument_slots(target)?;
        let stack_extra;
        if access.is_put() {
            let mut base = target_arguments.clone();
            base.push(descriptor.clone());
            let shape = callback_shape_for(
                target,
                &base,
                &callback.descriptor,
                "@Redirect at FIELD (write)",
                false,
            )?;
            if crate::return_desc(&callback.descriptor) != "V" {
                return Err(format!(
                    "{}: a write redirect callback must return void, but {} returns {}",
                    target.label(),
                    callback.descriptor,
                    crate::return_desc(&callback.descriptor)
                ));
            }
            if shape.takes_receiver && target.name == "<init>" {
                return Err(format!(
                    "{}: a write redirect taking the target receiver would pass an uninitialized `this`",
                    target.label()
                ));
            }
            let (opcode, width) = store_opcode(descriptor)?;
            let slot = target.code()?.max_locals;
            local_opcode(&mut bytes, opcode, slot);
            if !access.is_static() {
                bytes.push(0x57); // pop the field receiver
            }
            let (call, slots, _locals) =
                emit_callback(target, callback, shape, Some((slot, descriptor)), pool)?;
            bytes.extend(call);
            stack_extra = slots
                .checked_add(width)
                .and_then(|value| value.checked_add(1))
                .ok_or("argument slot overflow")?;
            local_slots = local_slots.max(slot.checked_add(width).ok_or("local slot overflow")?);
            frame_locals.push((slot, ValueType::from_descriptor(descriptor)?));
        } else {
            let shape = callback_shape_for(
                target,
                &target_arguments,
                &callback.descriptor,
                "@Redirect at FIELD (read)",
                false,
            )?;
            let expected = crate::return_desc(&callback.descriptor);
            if expected != descriptor {
                return Err(format!(
                    "{}: a read redirect callback must return the field type {descriptor}, but {} returns {expected}",
                    target.label(),
                    callback.descriptor
                ));
            }
            if shape.takes_receiver && target.name == "<init>" {
                return Err(format!(
                    "{}: a read redirect taking the target receiver would pass an uninitialized `this`",
                    target.label()
                ));
            }
            if !access.is_static() {
                bytes.push(0x57); // pop the field receiver
            }
            let (call, slots, locals) = emit_callback(target, callback, shape, None, pool)?;
            bytes.extend(call);
            stack_extra = slots.checked_add(1).ok_or("argument slot overflow")?;
            local_slots = local_slots.max(locals);
        }
        // A redirect replaces the access's stack effect (a read turns the
        // receiver into the value, a write consumes the operands), so the
        // original instruction is a `nop` and only the insert's entry keeps a
        // frame; the instruction itself no longer starts a basic block.
        let frames = FrameSites::InsertOnly;
        Ok(SiteInsert {
            bytes,
            stack_extra,
            local_slots,
            frame_locals,
            entry: TargetEntry::Enter,
            frames,
            replace_instruction: true,
        })
    })
}

/// The insert that replaces one non-special `INVOKE` call.
///
/// The call's arguments are saved in fresh locals (the last one is on top, so
/// the stores run in reverse), the receiver is popped, the callback runs with
/// the *target method's* arguments, and the original call is left as `nop`s.
/// The callback's return type is the call's return type, so the stack carries
/// the callback's result exactly where the call's result would have gone.
fn redirect_invoke_insert(
    target: &Target<'_>,
    callback: &MethodRef,
    descriptor: &str,
    receiver: bool,
    special: bool,
    pool: &mut PoolBuilder<'_>,
) -> Result<SiteInsert, String> {
    if special {
        return Err(format!(
            "{}: @Redirect at INVOKE cannot replace an invokespecial call; its receiver may be an uninitialized object",
            target.label()
        ));
    }
    if crate::return_desc(&callback.descriptor) != crate::return_desc(descriptor) {
        return Err(format!(
            "{}: @Redirect at INVOKE callback {} returns {}, but the call {descriptor} returns {}",
            target.label(),
            callback.descriptor,
            crate::return_desc(&callback.descriptor),
            crate::return_desc(descriptor)
        ));
    }
    let target_arguments = crate::descriptor_arguments(&target.descriptor);
    let shape = callback_shape_for(
        target,
        &target_arguments,
        &callback.descriptor,
        "@Redirect at INVOKE",
        false,
    )?;
    if shape.takes_receiver && target.name == "<init>" {
        return Err(format!(
            "{}: @Redirect at INVOKE taking the target receiver would pass an uninitialized `this`",
            target.label()
        ));
    }
    let call_arguments = crate::descriptor_arguments(descriptor);
    let mut bytes = Vec::new();
    let mut frame_locals = Vec::new();
    let mut local_slots = argument_slots(target)?;
    let mut slot = target.code()?.max_locals;
    let mut captured = Vec::new();
    for argument in &call_arguments {
        let (_, width) = store_opcode(argument)?;
        captured.push((slot, argument.clone()));
        slot = slot.checked_add(width).ok_or("local slot overflow")?;
    }
    // The last call argument is on top: store in reverse, keep each slot.
    for (captured_slot, argument) in captured.iter().rev() {
        let (opcode, _) = store_opcode(argument)?;
        local_opcode(&mut bytes, opcode, *captured_slot);
    }
    if receiver {
        bytes.push(0x57); // pop the call receiver
    }
    let (call, slots, locals) = emit_callback(target, callback, shape, None, pool)?;
    bytes.extend(call);
    for (captured_slot, argument) in &captured {
        local_slots = local_slots.max(
            captured_slot
                .checked_add(store_opcode(argument)?.1)
                .ok_or("local slot overflow")?,
        );
        frame_locals.push((*captured_slot, ValueType::from_descriptor(argument)?));
    }
    local_slots = local_slots.max(locals);
    let call_slots = descriptor_argument_slots(descriptor)?
        .checked_add(u16::from(receiver))
        .ok_or("argument slot overflow")?;
    Ok(SiteInsert {
        bytes,
        stack_extra: slots
            .checked_add(call_slots)
            .and_then(|value| value.checked_add(2))
            .ok_or("argument slot overflow")?,
        local_slots,
        frame_locals,
        entry: TargetEntry::Enter,
        frames: FrameSites::InsertOnly,
        replace_instruction: true,
    })
}

/// Apply one [`AnchorRequest`] to `class`.
///
/// `INVOKE` without `redirect` delegates to the existing call-site weaver
/// (including its ordinal/slice/shift handling); `HEAD` with `redirect`
/// replaces the body; and every other combination is a FIELD/NEW/JUMP insert or
/// a FIELD/INVOKE redirect as documented on [`weave_anchor_inject`] and
/// [`weave_anchor_redirect`].
pub fn weave_anchor(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    request: &AnchorRequest,
) -> Result<Vec<u8>, String> {
    match &request.site {
        AnchorSite::Head => {
            validate_head_modifiers(class, target, request)?;
            if request.redirect {
                weave_overwrite(class, target, callback)
            } else {
                weave_head_shape(class, target, callback, request.shift)
            }
        }
        AnchorSite::Invoke(selector) => {
            if request.redirect {
                return weave_anchor_redirect(class, target, callback, request);
            }
            let mut selector = selector.clone();
            selector.ordinal = request.ordinal;
            selector.slice = request.slice.clone();
            match request.shift {
                Some(shift) => weave_invoke_shifted_by(
                    class,
                    target,
                    callback,
                    &selector,
                    shift,
                    request.cancellable,
                ),
                None => {
                    weave_invoke_selected(class, target, callback, &selector, request.cancellable)
                }
            }
        }
        _ if request.redirect => weave_anchor_redirect(class, target, callback, request),
        _ => weave_anchor_inject(class, target, callback, request),
    }
}

/// Validate the slice and ordinal of a HEAD request without rewriting.
fn validate_head_modifiers(
    class: &ClassFile,
    target_ref: &MethodRef,
    request: &AnchorRequest,
) -> Result<(), String> {
    if let Some(ordinal) = request.ordinal {
        ordinal_index(
            &format!("{}.{}", target_ref.owner, target_ref.name),
            "HEAD",
            1,
            ordinal,
        )?;
    }
    if let Some(slice) = &request.slice {
        let target = prepare_target(class, target_ref)?;
        let label = target.label();
        let code = target.code()?;
        let scan = scan_code(&label, code.code)?;
        let (from, to) = resolve_slice(&label, &target, code, &scan, slice)?;
        if from > 0 || to == 0 {
            return Err(format!(
                "{label}: the slice from {} to {} does not contain the HEAD site at offset 0",
                slice.from.describe(),
                slice.to.describe()
            ));
        }
    }
    Ok(())
}

/// Apply one discovery anchor plan.
pub fn weave_anchor_plan(class: &ClassFile, plan: &MixinAnchorPlan) -> Result<Vec<u8>, String> {
    let request = AnchorRequest::from_plan(plan)?;
    weave_anchor(class, &plan.target, &plan.callback, &request)
}

/// Apply every anchor plan that targets `class`, in declaration order,
/// re-parsing between rewrites so each plan sees the previous ones.
///
/// A `@Redirect` at HEAD is the whole-body replacement [`weave_overwrite`]
/// emits; mixing one with other plans on the same target is a reason, because
/// the body it replaces is where those plans would have inserted.
pub fn weave_anchor_plans(class: &ClassFile, plans: &[MixinAnchorPlan]) -> Result<Vec<u8>, String> {
    let matching = plans
        .iter()
        .filter(|plan| plan.target.owner == class.name)
        .collect::<Vec<_>>();
    for plan in &matching {
        if plan.redirect
            && plan.at == "HEAD"
            && matching.iter().any(|other| {
                other.target.name == plan.target.name
                    && other.target.descriptor == plan.target.descriptor
                    && !std::ptr::eq(*other, *plan)
            })
        {
            return Err(format!(
                "{}.{}: a HEAD @Redirect replaces the whole body and cannot run alongside another anchor on the same target",
                plan.target.owner, plan.target.name
            ));
        }
    }
    let mut current = class.write()?;
    for plan in matching {
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        current = weave_anchor_plan(&next, plan)?;
    }
    Ok(current)
}

/// Replace a method body with one call to `callback`.
///
/// The callback must return the target's return type and consume the target's
/// arguments, optionally preceded by the target instance.  The rewritten code
/// is just the argument loads, the `invokestatic` and the matching `xreturn`;
/// the original exception table, debug tables and stack-map frames are dropped
/// because none of their offsets exist any more.  Constructors and static
/// initializers are rejected: a constructor still has to call `super`, which a
/// whole-body call cannot do.
pub fn weave_overwrite(
    class: &ClassFile,
    target_ref: &MethodRef,
    callback: &MethodRef,
) -> Result<Vec<u8>, String> {
    let request = AnchorRequest {
        site: AnchorSite::Head,
        ordinal: None,
        slice: None,
        shift: None,
        redirect: true,
        cancellable: false,
    };
    validate_head_modifiers(class, target_ref, &request)?;
    let target = prepare_target(class, target_ref)?;
    if target.name == "<init>" || target.name == "<clinit>" {
        return Err(format!(
            "{}: @Overwrite of a constructor or static initializer is not a bounded body replacement",
            target.label()
        ));
    }
    let arguments = crate::descriptor_arguments(&target.descriptor);
    let shape = callback_shape_for(
        &target,
        &arguments,
        &callback.descriptor,
        "@Overwrite",
        false,
    )?;
    let expected = crate::return_desc(&target.descriptor);
    let actual = crate::return_desc(&callback.descriptor);
    if actual != expected {
        return Err(format!(
            "{}: @Overwrite callback {} returns {actual}, but the target returns {expected}",
            target.label(),
            callback.descriptor
        ));
    }
    let mut pool = PoolBuilder::new(&class.pool);
    let mut bytes = Vec::new();
    let slots = emit_argument_loads(&target, shape.takes_receiver, &mut bytes)?;
    let index = pool.method_ref(callback)?;
    bytes.push(0xb8);
    bytes.extend_from_slice(&index.to_be_bytes());
    bytes.push(return_opcode(expected)?);
    let max_stack = slots.max(1);
    let max_locals = target.code()?.max_locals.max(argument_slots(&target)?);
    emit(
        &target.layout,
        Some(Patch {
            method: target.method_index,
            attribute: target.attribute_index,
            code: CodePatch {
                code: bytes,
                max_stack,
                max_locals,
                exception_table: Vec::new(),
                attributes: Vec::new(),
            },
        }),
        &pool.added,
    )
}

/// One selected `@ModifyArg` call site.
struct ModifyArgSite {
    at: usize,
    descriptor: String,
    /// Whether the selected call pushes a receiver below its arguments.
    receiver: bool,
}

fn modify_arg_sites(
    label: &str,
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
    plan: &MixinModifyArgPlan,
) -> Result<Vec<ModifyArgSite>, String> {
    if plan.invoke_target.descriptor.is_empty() {
        return Err(format!(
            "{label}: @ModifyArg needs the invoked call's full descriptor; {}.{}{} has none",
            plan.invoke_target.owner, plan.invoke_target.name, plan.invoke_target.descriptor
        ));
    }
    let mut sites = invoke_candidates(target, code, scan, &plan.invoke_target)?;
    if sites.is_empty() {
        return Err(format!(
            "{label}: @ModifyArg found no {}{} call site",
            plan.invoke_target.owner, plan.invoke_target.name
        ));
    }
    if let Some(ordinal) = plan.ordinal {
        if ordinal >= sites.len() {
            return Err(ordinal_out_of_range(
                label,
                "ModifyArg INVOKE",
                sites.len(),
                i32::try_from(ordinal).unwrap_or(i32::MAX),
            ));
        }
        sites = vec![sites.remove(ordinal)];
    }
    Ok(sites
        .into_iter()
        .map(|site| ModifyArgSite {
            at: site.at,
            descriptor: site.descriptor,
            receiver: site.receiver,
        })
        .collect())
}

/// The stack-and-local edits that insert one argument rewrite.
fn build_modify_arg_insert(
    target: &Target<'_>,
    callback: &MethodRef,
    descriptor: &str,
    index: usize,
    receiver: bool,
    temp_base: u16,
    pool: &mut PoolBuilder<'_>,
) -> Result<SiteInsert, String> {
    let arguments = crate::descriptor_arguments(descriptor);
    let Some(value) = arguments.get(index).cloned() else {
        return Err(format!(
            "{}: @ModifyArg index {index} is outside the call descriptor {descriptor}",
            target.label()
        ));
    };
    let callback_arguments = crate::descriptor_arguments(&callback.descriptor);
    let simple = callback_arguments.len() == 1 && callback_arguments[0] == value;
    let full = callback_arguments == arguments;
    if !simple && !full {
        return Err(format!(
            "{}: @ModifyArg callback {} must take the modified {value} alone or mirror the call's arguments {:?}",
            target.label(),
            callback.descriptor,
            arguments
        ));
    }
    if crate::return_desc(&callback.descriptor) != value {
        return Err(format!(
            "{}: @ModifyArg callback {} returns {} but argument {index} of the call is {value}",
            target.label(),
            callback.descriptor,
            crate::return_desc(&callback.descriptor)
        ));
    }

    let mut bytes = Vec::new();
    let mut frame_locals = Vec::new();
    let mut local_slots = argument_slots(target)?;
    let args_slots = descriptor_argument_slots(descriptor)?
        .checked_add(u16::from(receiver))
        .ok_or("argument slot overflow")?;
    let mut slot = temp_base;
    let mut stack_extra = args_slots.saturating_add(2);

    if simple {
        let mut saved = Vec::new();
        for argument in arguments.iter().skip(index + 1) {
            let (_, width) = store_opcode(argument)?;
            saved.push((slot, argument.clone()));
            slot = slot.checked_add(width).ok_or("local slot overflow")?;
        }
        // The operand stack has the last argument on top, so the stores run in
        // reverse order while each value keeps the slot its position names.
        for (saved_slot, argument) in saved.iter().rev() {
            let (opcode, _) = store_opcode(argument)?;
            local_opcode(&mut bytes, opcode, *saved_slot);
        }
        let callback_index = pool.method_ref(callback)?;
        bytes.push(0xb8);
        bytes.extend_from_slice(&callback_index.to_be_bytes());
        for (saved_slot, argument) in &saved {
            let (opcode, _) = load_opcode(argument)?;
            local_opcode(&mut bytes, opcode, *saved_slot);
        }
        for (saved_slot, argument) in saved {
            local_slots = local_slots.max(
                saved_slot
                    .checked_add(store_opcode(&argument)?.1)
                    .ok_or("local slot overflow")?,
            );
            frame_locals.push((saved_slot, ValueType::from_descriptor(&argument)?));
        }
        local_slots = local_slots.max(slot);
        // The callback consumes the value that remains on the stack and leaves
        // its replacement there, so the peak is the call's own argument slots.
        stack_extra = stack_extra.max(args_slots.saturating_add(1));
    } else {
        let mut captured = Vec::new();
        for argument in arguments.iter() {
            let (_, width) = store_opcode(argument)?;
            captured.push((slot, argument.clone()));
            slot = slot.checked_add(width).ok_or("local slot overflow")?;
        }
        // Last argument on top: store in reverse, keep each argument's slot.
        for (captured_slot, argument) in captured.iter().rev() {
            let (opcode, _) = store_opcode(argument)?;
            local_opcode(&mut bytes, opcode, *captured_slot);
        }
        for (captured_slot, argument) in &captured {
            let (opcode, _) = load_opcode(argument)?;
            local_opcode(&mut bytes, opcode, *captured_slot);
        }
        let callback_index = pool.method_ref(callback)?;
        bytes.push(0xb8);
        bytes.extend_from_slice(&callback_index.to_be_bytes());
        let temp = slot;
        let (store, width) = store_opcode(&value)?;
        local_opcode(&mut bytes, store, temp);
        for (position, (captured_slot, argument)) in captured.iter().enumerate() {
            if position == index {
                let (load, _) = load_opcode(&value)?;
                local_opcode(&mut bytes, load, temp);
            } else {
                let (load, _) = load_opcode(argument)?;
                local_opcode(&mut bytes, load, *captured_slot);
            }
        }
        for (captured_slot, argument) in &captured {
            local_slots = local_slots.max(
                captured_slot
                    .checked_add(store_opcode(argument)?.1)
                    .ok_or("local slot overflow")?,
            );
            frame_locals.push((*captured_slot, ValueType::from_descriptor(argument)?));
        }
        local_slots = local_slots.max(temp.checked_add(width).ok_or("local slot overflow")?);
        frame_locals.push((temp, ValueType::from_descriptor(&value)?));
        stack_extra = stack_extra.max(args_slots.saturating_add(2));
    }
    Ok(SiteInsert {
        bytes,
        stack_extra,
        local_slots,
        frame_locals,
        entry: TargetEntry::Enter,
        frames: FrameSites::InsertOnly,
        replace_instruction: false,
    })
}

/// The local slots a method descriptor's arguments occupy.
fn descriptor_argument_slots(descriptor: &str) -> Result<u16, String> {
    let mut slots = 0u16;
    for argument in crate::descriptor_arguments(descriptor) {
        let (_, width) = load_opcode(&argument)?;
        slots = slots.checked_add(width).ok_or("argument slot overflow")?;
    }
    Ok(slots)
}

/// Apply one `@ModifyArg` plan.
///
/// The callback runs immediately before the selected call and its return value
/// replaces argument `index`.  A simple callback takes that argument alone; the
/// insert saves the arguments above it in fresh locals, calls the callback and
/// loads them back.  A full callback mirrors the whole argument list; the
/// insert saves every argument, calls the callback, and reloads the list with
/// the result in place of `index`.
pub fn weave_modify_arg(class: &ClassFile, plan: &MixinModifyArgPlan) -> Result<Vec<u8>, String> {
    let target = prepare_target(class, &plan.target)?;
    let label = target.label();
    let code = target.code()?;
    let scan = scan_code(&label, code.code)?;
    let sites = modify_arg_sites(&label, &target, code, &scan, plan)?;
    let temp_base = code.max_locals;
    let mut current = class.write()?;
    for position in (0..sites.len()).rev() {
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        let target = prepare_target(&next, &plan.target)?;
        let label = target.label();
        let code = target.code()?;
        let scan = scan_code(&label, code.code)?;
        let selected = modify_arg_sites(&label, &target, code, &scan, plan)?;
        if selected.len() != sites.len() {
            return Err(format!(
                "{label}: the @ModifyArg call-site selection changed while weaving"
            ));
        }
        let site = &selected[position];
        let mut pool = PoolBuilder::new(&next.pool);
        let insert = build_modify_arg_insert(
            &target,
            &plan.callback,
            &site.descriptor,
            plan.index,
            site.receiver,
            temp_base,
            &mut pool,
        )?;
        current = insert_once(&next, &plan.target, site.at, insert, &mut pool)?;
    }
    Ok(current)
}

/// One selected `@ModifyVariable` local access.
struct LocalSite {
    at: usize,
    /// The offset of the instruction after the access, where a LOAD hook goes.
    next: usize,
    slot: u16,
}

/// Decode one local load/store instruction.
fn local_access(
    label: &str,
    code: &[u8],
    at: usize,
    opcode: u8,
) -> Result<Option<(bool, u16, ValueType)>, String> {
    let (store, slot, value) = match opcode {
        0x15..=0x19 => (
            false,
            code_u1(code, at + 1)? as u16,
            match opcode {
                0x15 => ValueType::Int,
                0x16 => ValueType::Long,
                0x17 => ValueType::Float,
                0x18 => ValueType::Double,
                _ => ValueType::Object("java/lang/Object".to_owned()),
            },
        ),
        0x1a..=0x1d => (false, (opcode - 0x1a) as u16, ValueType::Int),
        0x1e..=0x21 => (false, (opcode - 0x1e) as u16, ValueType::Long),
        0x22..=0x25 => (false, (opcode - 0x22) as u16, ValueType::Float),
        0x26..=0x29 => (false, (opcode - 0x26) as u16, ValueType::Double),
        0x2a..=0x2d => (
            false,
            (opcode - 0x2a) as u16,
            ValueType::Object("java/lang/Object".to_owned()),
        ),
        0x36..=0x3a => (
            true,
            code_u1(code, at + 1)? as u16,
            match opcode {
                0x36 => ValueType::Int,
                0x37 => ValueType::Long,
                0x38 => ValueType::Float,
                0x39 => ValueType::Double,
                _ => ValueType::Object("java/lang/Object".to_owned()),
            },
        ),
        0x3b..=0x3e => (true, (opcode - 0x3b) as u16, ValueType::Int),
        0x3f..=0x42 => (true, (opcode - 0x3f) as u16, ValueType::Long),
        0x43..=0x46 => (true, (opcode - 0x43) as u16, ValueType::Float),
        0x47..=0x4a => (true, (opcode - 0x47) as u16, ValueType::Double),
        0x4b..=0x4e => (
            true,
            (opcode - 0x4b) as u16,
            ValueType::Object("java/lang/Object".to_owned()),
        ),
        0xc4 => {
            let sub = code_u1(code, at + 1)?;
            let index = code_u2(code, at + 2)?;
            let (store, value) = match sub {
                0x15 => (false, ValueType::Int),
                0x16 => (false, ValueType::Long),
                0x17 => (false, ValueType::Float),
                0x18 => (false, ValueType::Double),
                0x19 => (false, ValueType::Object("java/lang/Object".to_owned())),
                0x36 => (true, ValueType::Int),
                0x37 => (true, ValueType::Long),
                0x38 => (true, ValueType::Float),
                0x39 => (true, ValueType::Double),
                0x3a => (true, ValueType::Object("java/lang/Object".to_owned())),
                _ => return Ok(None),
            };
            (store, index, value)
        }
        _ => return Ok(None),
    };
    let _ = label;
    Ok(Some((store, slot, value)))
}

fn value_matches(callback: &ValueType, local: &ValueType) -> bool {
    match (callback, local) {
        (ValueType::Object(_), ValueType::Object(_)) => true,
        (ValueType::Object(_), ValueType::Null) => true,
        (a, b) => a == b,
    }
}

fn local_sites(
    label: &str,
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
    plan: &MixinModifyVariablePlan,
    callback_value: &ValueType,
) -> Result<Vec<LocalSite>, String> {
    let want_store = plan.at == "STORE";
    let mut sites = Vec::new();
    for (index, instruction) in scan.instructions.iter().enumerate() {
        let opcode = code_u1(code.code, instruction.at)?;
        let Some((store, slot, value)) = local_access(label, code.code, instruction.at, opcode)?
        else {
            continue;
        };
        if store != want_store || !value_matches(callback_value, &value) {
            continue;
        }
        let Some(next) = scan.instructions.get(index + 1).map(|next| next.at) else {
            continue;
        };
        sites.push(LocalSite {
            at: instruction.at,
            next,
            slot,
        });
    }
    if let Some(index) = plan.index {
        let slot = u16::try_from(index).map_err(|_| {
            format!("{label}: @ModifyVariable index {index} exceeds the local-slot limit")
        })?;
        sites.retain(|site| site.slot == slot);
    } else if let Some(ordinal) = plan.ordinal {
        if ordinal >= sites.len() {
            return Err(format!(
                "{label}: @ModifyVariable ordinal {ordinal} is outside the {} matching {} site(s)",
                sites.len(),
                plan.at
            ));
        }
        sites = vec![sites.remove(ordinal)];
    } else {
        return Err(format!(
            "{label}: @ModifyVariable needs an index or an ordinal; implicit capture is not modeled"
        ));
    }
    if sites.is_empty() {
        return Err(format!(
            "{label}: @ModifyVariable matches no {} of a value the callback accepts",
            plan.at
        ));
    }
    let _ = target;
    Ok(sites)
}

fn build_modify_variable_insert(
    target: &Target<'_>,
    callback: &MethodRef,
    callback_value: &ValueType,
    _site: &LocalSite,
    temp_base: u16,
    pool: &mut PoolBuilder<'_>,
) -> Result<SiteInsert, String> {
    let callback_arguments = crate::descriptor_arguments(&callback.descriptor);
    let Some(first) = callback_arguments.first() else {
        return Err(format!(
            "{}: @ModifyVariable callback {} takes no value",
            target.label(),
            callback.descriptor
        ));
    };
    let expected = match callback_value {
        ValueType::Int => "I".to_owned(),
        ValueType::Long => "J".to_owned(),
        ValueType::Float => "F".to_owned(),
        ValueType::Double => "D".to_owned(),
        ValueType::Object(_) => first.clone(),
        other => {
            return Err(format!(
                "{}: @ModifyVariable cannot match a {other:?} local",
                target.label()
            ));
        }
    };
    if crate::return_desc(&callback.descriptor) != expected {
        return Err(format!(
            "{}: @ModifyVariable callback {} returns {}, not the {expected} it captures",
            target.label(),
            callback.descriptor,
            crate::return_desc(&callback.descriptor)
        ));
    }
    let target_arguments = crate::descriptor_arguments(&target.descriptor);
    let captures = &callback_arguments[1..];
    if !captures.is_empty() && captures != target_arguments.as_slice() {
        return Err(format!(
            "{}: @ModifyVariable callback {} must take the value alone or followed by the target arguments {target_arguments:?}",
            target.label(),
            callback.descriptor
        ));
    }
    let mut bytes = Vec::new();
    let mut frame_locals = Vec::new();
    let mut local_slots = argument_slots(target)?;
    let stack_extra = if captures.is_empty() {
        let index = pool.method_ref(callback)?;
        bytes.push(0xb8);
        bytes.extend_from_slice(&index.to_be_bytes());
        2
    } else {
        let (store, width) = store_opcode(&expected)?;
        local_opcode(&mut bytes, store, temp_base);
        // The callback's first argument is the value, then the target's
        // arguments, so the value is loaded before them.
        let (load, _) = load_opcode(&expected)?;
        local_opcode(&mut bytes, load, temp_base);
        let pushes = emit_argument_loads(target, false, &mut bytes)?;
        let index = pool.method_ref(callback)?;
        bytes.push(0xb8);
        bytes.extend_from_slice(&index.to_be_bytes());
        local_slots = local_slots.max(temp_base.checked_add(width).ok_or("local slot overflow")?);
        let frame_value = ValueType::from_descriptor(&expected)?;
        frame_locals.push((temp_base, frame_value));
        pushes
            .checked_add(width)
            .and_then(|value| value.checked_add(2))
            .ok_or("argument slot overflow")?
    };
    let frames = if frame_locals.is_empty() {
        FrameSites::Both
    } else {
        FrameSites::InsertOnly
    };
    Ok(SiteInsert {
        bytes,
        stack_extra,
        local_slots,
        frame_locals,
        entry: TargetEntry::Enter,
        frames,
        replace_instruction: false,
    })
}

/// Whether any `StackMapTable` frame sits at or after `offset`.
fn has_frame_at_or_after(code: &CodeLayout<'_>, offset: usize) -> bool {
    code.attributes.iter().any(|attribute| match attribute {
        CodeAttribute::StackMap { frames, .. } => frames.iter().any(|frame| frame.offset >= offset),
        _ => false,
    })
}

/// Apply one `@ModifyVariable` plan.
///
/// At `STORE` the callback runs before the store, so its return value is what
/// the local receives.  At `LOAD` it runs after the load, so it replaces the
/// loaded value on the operand stack.  A callback that also declares the target
/// arguments is supported for a store; the same shape at a load is rejected
/// when a later stack-map frame exists, because a branch that skips the load
/// would reach the frame without the insert's fresh local and a single frame
/// cannot describe both paths.
pub fn weave_modify_variable(
    class: &ClassFile,
    plan: &MixinModifyVariablePlan,
) -> Result<Vec<u8>, String> {
    if plan.at != "STORE" && plan.at != "LOAD" {
        return Err(format!(
            "{}.{}: @ModifyVariable anchor {} is not bounded; use STORE or LOAD",
            plan.target.owner, plan.target.name, plan.at
        ));
    }
    let callback_arguments = crate::descriptor_arguments(&plan.callback.descriptor);
    let Some(first) = callback_arguments.first() else {
        return Err(format!(
            "@ModifyVariable callback {} takes no value",
            plan.callback.descriptor
        ));
    };
    let callback_value = ValueType::from_descriptor(first)?;
    if !callback_arguments[1..].is_empty() {
        // Capturing the target's arguments is accepted for a store; check the
        // load shape against the frames before any edit.
        if plan.at == "LOAD" {
            let target = prepare_target(class, &plan.target)?;
            let label = target.label();
            let code = target.code()?;
            let scan = scan_code(&label, code.code)?;
            let sites = local_sites(&label, &target, code, &scan, plan, &callback_value)?;
            if sites
                .iter()
                .any(|site| has_frame_at_or_after(code, site.next))
            {
                return Err(format!(
                    "{label}: @ModifyVariable at LOAD with a captured argument list would leave fresh locals on only the fallthrough path into a stack-map frame; the bounded weaver cannot describe both paths"
                ));
            }
        }
    }

    let target = prepare_target(class, &plan.target)?;
    let label = target.label();
    let code = target.code()?;
    let scan = scan_code(&label, code.code)?;
    let sites = local_sites(&label, &target, code, &scan, plan, &callback_value)?;
    let temp_base = code.max_locals;
    let mut current = class.write()?;
    for position in (0..sites.len()).rev() {
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        let target = prepare_target(&next, &plan.target)?;
        let label = target.label();
        let code = target.code()?;
        let scan = scan_code(&label, code.code)?;
        let selected = local_sites(&label, &target, code, &scan, plan, &callback_value)?;
        if selected.len() != sites.len() {
            return Err(format!(
                "{label}: the @ModifyVariable local selection changed while weaving"
            ));
        }
        let site = &selected[position];
        let mut pool = PoolBuilder::new(&next.pool);
        let mut insert = build_modify_variable_insert(
            &target,
            &plan.callback,
            &callback_value,
            site,
            temp_base,
            &mut pool,
        )?;
        let at = if plan.at == "STORE" {
            insert.entry = TargetEntry::Enter;
            site.at
        } else {
            // A LOAD callback runs after the load.  `Skip` keeps a branch that
            // targets the next instruction from entering the insert, while
            // straight-line execution falls through it.
            insert.entry = TargetEntry::Skip;
            site.next
        };
        current = insert_once(&next, &plan.target, at, insert, &mut pool)?;
    }
    Ok(current)
}

///
/// `class` must be the class that declares `target` (`target.owner` must equal
/// [`ClassFile::name`]).  The returned bytes are the rewritten class; the input
/// is untouched.
///
/// The callback is invoked with `invokestatic`, so it must be static.  A
/// callback takes the target's arguments in order and returns `void`.  An
/// instance target's callback may take the target instance first, which is
/// what the inserted `aload_0` supplies; a callback that does not declare it
/// gets the arguments only, and no receiver is pushed.  A trailing plain
/// `CallbackInfo` is constructed by the insert and passed as the last
/// argument; [`weave_classes_with_extras`] is how an instance callback (which
/// cannot be `invokestatic`ed) gets the synthetic bridge that reaches it.
///
/// The insert moves every branch target, switch table, exception range,
/// stack-map frame and debug-table offset after it; see [`crate::weaver`] for
/// the rewrite.
///
/// Errors name the reason a method is outside the subset instead of guessing:
/// malformed condy, abstract/native methods, a mismatched callback descriptor,
/// a trailing `CallbackInfoReturnable` on a non-return anchor, a `wide` prefix
/// that is not a local-variable instruction, or a conditional branch the split
/// cannot express.
pub fn weave_head(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
) -> Result<Vec<u8>, String> {
    weave_head_shape(class, target, callback, None)
}

/// The shared body of [`weave_head`], [`weave_head_selected`] and
/// [`weave_head_shifted`].
fn weave_head_shape(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    shift: Option<ShiftBy>,
) -> Result<Vec<u8>, String> {
    let target = prepare_target(class, target)?;
    let label = target.label();
    let shape = callback_arguments(
        &target.class_name,
        target.is_static,
        &target.descriptor,
        &callback.descriptor,
        "HEAD",
        Injected::None,
    )?;

    let mut pool = PoolBuilder::new(&class.pool);
    let call = if shape.callback_info {
        plain_callback_info_call(&target, callback, shape.takes_receiver, &mut pool)?
    } else {
        callback_call(&target, callback, shape.takes_receiver, &mut pool)?
    };
    let code = target.code()?;
    let scan = scan_code(&label, code.code)?;
    let insert_at = match shift {
        Some(shift) => {
            let delta = shift.distance(&label)?;
            if delta == 0 {
                0
            } else {
                shifted_offset(&label, code, &scan, 0, delta)?
            }
        }
        None => 0,
    };
    let entry = if insert_at == 0 {
        TargetEntry::Skip
    } else {
        TargetEntry::Enter
    };
    if code.code.len() + call.bytes.len() > MAX_CODE_LENGTH {
        return Err(format!(
            "{label}: woven code would exceed the class-file limit of {MAX_CODE_LENGTH} bytes"
        ));
    }
    let max_stack = u32::from(code.max_stack)
        .checked_add(u32::from(call.stack_slots))
        .and_then(|value| value.checked_add(1))
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| format!("{label}: woven max_stack overflows u16"))?;
    let max_locals = code.max_locals.max(call.local_slots);
    let patch = relocate(
        &label,
        &target,
        code,
        &scan,
        &[(insert_at, call.bytes.len())],
        &call.bytes,
        max_stack,
        max_locals,
        entry,
        FrameSites::Both,
        &[],
        &mut pool,
    )?;

    emit(
        &target.layout,
        Some(Patch {
            method: target.method_index,
            attribute: target.attribute_index,
            code: patch,
        }),
        &pool.added,
    )
}

/// Insert the `HEAD` callback only when `ordinal` names the method's single
/// `HEAD` site, which is offset 0.
///
/// `None`, `0` and `-1` all select it; any other ordinal is a reason, because a
/// slice cannot put a second `HEAD` site in the method.  The rewrite itself is
/// [`weave_head`]: same callback shape, same relocation, same rejections.
pub fn weave_head_selected(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    ordinal: Option<i32>,
) -> Result<Vec<u8>, String> {
    if let Some(ordinal) = ordinal {
        ordinal_index(
            &format!("{}.{}", target.owner, target.name),
            "HEAD",
            1,
            ordinal,
        )?;
    }
    weave_head(class, target, callback)
}

/// Insert the `HEAD` callback a signed [`Shift::By`] distance from offset 0.
///
/// Positive `by` moves the insert into the method body, negative is a reason
/// (offset 0 has no earlier instruction), and `0` is byte-for-byte
/// [`weave_head`].  `ordinal` is validated exactly as [`weave_head_selected`]
/// does.  The insert is *entered* once it moves off offset 0, so a branch to
/// the shifted instruction runs the callback first.
pub fn weave_head_shifted(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    ordinal: Option<i32>,
    shift: ShiftBy,
) -> Result<Vec<u8>, String> {
    if let Some(ordinal) = ordinal {
        ordinal_index(
            &format!("{}.{}", target.owner, target.name),
            "HEAD",
            1,
            ordinal,
        )?;
    }
    weave_head_shape(class, target, callback, Some(shift))
}

/// Insert a callback immediately before the target's `return` instructions.
///
/// [`ReturnMode::Return`] inserts before every return-family instruction in
/// the method; [`ReturnMode::Tail`] inserts before the last one only, matching
/// Mixin's `RETURN` and `TAIL` anchors.
///
/// A `void` target weaves the plain `invokestatic`.  A non-void target also
/// carries its result: the callback must end with a `CallbackInfoReturnable`,
/// which the insert builds from the target's boxed value, and the method then
/// returns `getReturnValue()` in its own shape, so a `setReturnValue` override
/// wins and the target's value survives when none is set.  A `cancel()` keeps
/// the override rather than discarding it - the method returns
/// `getReturnValue()` either way, which is what Mixin's own cancellable
/// `RETURN` handler does.  The object [`callback_info_returnable_class`] writes
/// is the class such a jar needs.
///
/// The callback is invoked with `invokestatic`.  A callback takes the target's
/// arguments in order and returns `void`.  An instance target's callback may
/// take the target instance first, which the inserted `aload_0` supplies.  A
/// trailing plain `CallbackInfo` is constructed by the insert and passed; the
/// result stays on the operand stack underneath the inserted values, so the
/// target still returns its own value.  Each insert relocates the offsets after
/// it exactly as [`weave_head`] does, so a branchy method, a loop, a
/// `tableswitch`/`lookupswitch` (whose padding is re-derived from its new
/// offset), a `wide` local and an exception handler keep their targets and
/// frames.  Unlike a `HEAD` insert, a `RETURN` insert is entered by the
/// branches and switch cases that named the `return`.
/// Every other rejection matches
/// [`weave_head`]: malformed condy, abstract/native methods, a mismatched
/// callback descriptor, or a non-void target whose callback ends with a plain
/// `CallbackInfo` instead of a `CallbackInfoReturnable`.
pub fn weave_return(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    mode: ReturnMode,
) -> Result<Vec<u8>, String> {
    weave_return_shape(class, target, callback, mode, None, None, None)
}

/// Insert a callback before the one return site a [`ReturnSelector`] names.
///
/// `non_void` says the callback observes the target's result through the
/// trailing `CallbackInfoReturnable`, which the target's descriptor has to
/// confirm: a `void` target has no result, so the two disagree.  With
/// `non_void == false` the insert is the stack-neutral plain callback a `void`
/// target uses; a non-void target accepts it too, because the insert leaves the
/// result on the operand stack untouched, but then the callback cannot observe
/// or replace it - that is what `non_void == true` is for.
///
/// The selector counts the return instructions inside its slice, 0-based, with
/// a negative ordinal counted from the end (`-1` is the last).  [`ReturnMode`]
/// decides whether every counted return is an insertion site ([`ReturnMode::Return`])
/// or only the last one ([`ReturnMode::Tail`]); an ordinal then picks one of
/// them.  An ordinal outside the counted sites, a slice that names no single
/// boundary, and a slice that holds no return are reasons, not guesses.
pub fn weave_returns_selected(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    non_void: bool,
    selector: &ReturnSelector,
) -> Result<Vec<u8>, String> {
    weave_return_shape(
        class,
        target,
        callback,
        selector.mode,
        Some(selector),
        Some(non_void),
        None,
    )
}

/// Insert the callback a signed [`Shift::By`] distance from every return site
/// the [`ReturnSelector`] names: positive moves towards the end of the method,
/// negative towards its start, and `0` keeps the selectors' own return offsets
/// byte for byte.
///
/// The selector (slice and ordinal) picks the returns first, exactly as
/// [`weave_returns_selected`] does; the shift then moves each chosen insert.
/// A shift past either end of the method, or one that would land strictly
/// inside an exception range, is a reason.  A result-observing insert
/// (`non_void`) shifted off its return expects the result on the operand stack
/// where it lands, which only a point inside the return's own straight-line
/// arm can promise; the caller owns that shape.
pub fn weave_returns_shifted(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    non_void: bool,
    selector: &ReturnSelector,
    shift: ShiftBy,
) -> Result<Vec<u8>, String> {
    weave_return_shape(
        class,
        target,
        callback,
        selector.mode,
        Some(selector),
        Some(non_void),
        Some(shift),
    )
}

/// The shared body of [`weave_return`], [`weave_returns_selected`] and
/// [`weave_returns_shifted`].
fn weave_return_shape(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    mode: ReturnMode,
    selector: Option<&ReturnSelector>,
    observed_non_void: Option<bool>,
    shift: Option<ShiftBy>,
) -> Result<Vec<u8>, String> {
    let target = prepare_target(class, target)?;
    let label = target.label();
    let anchor = mode.anchor();
    let returns_value = crate::return_desc(&target.descriptor) != "V";
    // `non_void` only decides the shape: `true` asks the insert to carry the
    // result, which needs a value-returning target; `false` keeps the plain,
    // stack-neutral callback a `void` target uses, which a value-returning
    // target accepts too.
    let observes_result = observed_non_void.unwrap_or(returns_value);
    if observes_result && !returns_value {
        return Err(format!(
            "{label}: non_void says the callback observes the result through a CallbackInfoReturnable, but the target's descriptor {} answers void",
            target.descriptor
        ));
    }
    let shape = callback_arguments(
        &target.class_name,
        target.is_static,
        &target.descriptor,
        &callback.descriptor,
        anchor,
        if observes_result {
            Injected::Returnable
        } else {
            Injected::None
        },
    )?;

    let code = target.code()?;
    let scan = scan_code(&label, code.code)?;
    let mut sites: Vec<usize> = scan.returns.clone();
    if let Some(slice) = selector.and_then(|selector| selector.slice.as_ref()) {
        let (from, to) = resolve_slice(&label, &target, code, &scan, slice)?;
        sites.retain(|at| *at >= from && *at < to);
        if sites.is_empty() {
            return Err(format!(
                "{label}: {anchor} weaving found no return instruction inside the slice from {} to {}",
                slice.from.describe(),
                slice.to.describe()
            ));
        }
    }
    if mode == ReturnMode::Tail {
        sites = sites.last().copied().into_iter().collect();
    }
    if let Some(ordinal) = selector.and_then(|selector| selector.ordinal) {
        let index = ordinal_index(&label, anchor, sites.len(), ordinal)?;
        sites = vec![sites[index]];
    }
    if sites.is_empty() {
        return Err(format!(
            "{label}: {anchor} weaving found no return instruction"
        ));
    }
    if let Some(shift) = shift {
        let delta = shift.distance(&label)?;
        if delta != 0 {
            sites = sites
                .iter()
                .map(|at| shifted_offset(&label, code, &scan, *at, delta))
                .collect::<Result<Vec<_>, String>>()?;
        }
    }

    let mut pool = PoolBuilder::new(&class.pool);
    // A result-observing insert keeps the boxed value in a fresh local past
    // every one the method declares, so it can never shadow an argument or a
    // local.
    let (call, frame_sites) = if observes_result {
        let slot = code.max_locals.max(argument_slots(&target)?);
        let call = returnable_call(&target, callback, shape.takes_receiver, slot, &mut pool)?;
        let sites = FrameSites::InsertOnly;
        (call, sites)
    } else if shape.callback_info {
        let call = plain_callback_info_call(&target, callback, shape.takes_receiver, &mut pool)?;
        (call, FrameSites::Both)
    } else {
        let call = callback_call(&target, callback, shape.takes_receiver, &mut pool)?;
        (call, FrameSites::Both)
    };
    if code.code.len() + call.bytes.len() * sites.len() > MAX_CODE_LENGTH {
        return Err(format!(
            "{label}: woven code would exceed the class-file limit of {MAX_CODE_LENGTH} bytes"
        ));
    }
    let max_stack = u32::from(code.max_stack)
        .checked_add(u32::from(call.stack_slots))
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| format!("{label}: woven max_stack overflows u16"))?;
    let max_locals = code.max_locals.max(call.local_slots);
    let insertions = sites
        .iter()
        .map(|site| (*site, call.bytes.len()))
        .collect::<Vec<_>>();
    let patch = relocate(
        &label,
        &target,
        code,
        &scan,
        &insertions,
        &call.bytes,
        max_stack,
        max_locals,
        TargetEntry::Enter,
        frame_sites,
        &[],
        &mut pool,
    )?;

    emit(
        &target.layout,
        Some(Patch {
            method: target.method_index,
            attribute: target.attribute_index,
            code: patch,
        }),
        &pool.added,
    )
}

/// Emit the shared prefix of a cancellable insert: construct one
/// `CallbackInfo`, call the callback with the target's arguments (and receiver,
/// when it declares it) plus that object, and leave the object on the stack.
///
/// Returns the bytes, the peak number of operand-stack slots they need, and
/// the highest local slot they read plus one.
fn callback_info_call(
    target: &Target<'_>,
    callback: &MethodRef,
    takes_receiver: bool,
    slot: u16,
    pool: &mut PoolBuilder<'_>,
) -> Result<(Vec<u8>, u16, u16), String> {
    let mut bytes = Vec::new();
    let info = pool.class(CALLBACK_INFO_CLASS)?;
    bytes.push(0xbb); // new CallbackInfo
    bytes.extend_from_slice(&info.to_be_bytes());
    bytes.push(0x59); // dup
    let init = pool.method_ref(&MethodRef {
        owner: CALLBACK_INFO_CLASS.to_owned(),
        name: "<init>".to_owned(),
        descriptor: "()V".to_owned(),
    })?;
    bytes.push(0xb7); // invokespecial CallbackInfo.<init>()V
    bytes.extend_from_slice(&init.to_be_bytes());
    local_opcode(&mut bytes, 0x3a, slot); // astore <slot>
    let mut slots = 0u16;
    let mut local_slots = slot
        .checked_add(1)
        .ok_or("the fresh local does not fit the class-file local-slot limit")?;
    for load in argument_loads(target, takes_receiver)? {
        load.emit(&mut bytes);
        let (index, width) = load.span();
        local_slots = local_slots.max(index.saturating_add(width));
        slots = slots.saturating_add(width);
    }
    local_opcode(&mut bytes, 0x19, slot); // aload <slot>
    let callback_index = pool.method_ref(callback)?;
    bytes.push(0xb8); // invokestatic callback
    bytes.extend_from_slice(&callback_index.to_be_bytes());
    local_opcode(&mut bytes, 0x19, slot); // aload <slot>
    Ok((bytes, slots.saturating_add(1).max(2), local_slots))
}

/// Emit one cancellable `void` `HEAD` insert.
///
/// The insert builds a `CallbackInfo`, calls the callback with the target's
/// arguments plus that object, and branches around the target body when the
/// callback cancelled.  A cancelled `void` method answers with its only
/// possible default: a plain `return`.
fn cancellable_head_call(
    target: &Target<'_>,
    callback: &MethodRef,
    takes_receiver: bool,
    slot: u16,
    pool: &mut PoolBuilder<'_>,
) -> Result<CallbackCall, String> {
    let (mut bytes, stack_slots, local_slots) =
        callback_info_call(target, callback, takes_receiver, slot, pool)?;
    let is_cancelled = pool.method_ref(&MethodRef {
        owner: CALLBACK_INFO_CLASS.to_owned(),
        name: "isCancelled".to_owned(),
        descriptor: "()Z".to_owned(),
    })?;
    bytes.push(0xb6); // invokevirtual isCancelled
    bytes.extend_from_slice(&is_cancelled.to_be_bytes());
    // `ifeq body; return`: the branch target is the first original instruction,
    // which the insert places right after these bytes, so the operand is the
    // fixed distance over the `return`.
    let ifeq_at = bytes.len();
    bytes.push(0x99); // ifeq
    let operand = bytes.len();
    bytes.extend_from_slice(&[0, 0]);
    bytes.push(0xb1); // return
    let body = bytes.len();
    let delta = i16::try_from(body as i64 - ifeq_at as i64)
        .map_err(|_| "the cancellable insert cannot reach its body".to_owned())?;
    bytes[operand..operand + 2].copy_from_slice(&delta.to_be_bytes());
    Ok(CallbackCall {
        bytes,
        // `new` + `dup` while the stack is empty, then the arguments and the
        // `CallbackInfo` at the call.
        stack_slots,
        local_slots,
    })
}

/// The `void` target's entry locals, extended with the fresh local the
/// cancellable insert stores the `CallbackInfo` in.
fn cancellable_entry_locals(target: &Target<'_>, slot: u16) -> Result<Vec<ValueType>, String> {
    let mut slots = slots_from_values(&initial_local_values(target)?);
    if slots.len() < slot as usize {
        slots.resize(slot as usize, ValueType::Top);
    }
    write_local(
        &mut slots,
        slot as usize,
        ValueType::Object(CALLBACK_INFO_CLASS.to_owned()),
    );
    Ok(slots)
}

/// Insert a cancellable `CallbackInfo` callback at offset 0 of a `void`
/// `target`.
///
/// The insert constructs [`callback_info_class`], calls the callback with the
/// target's arguments (and receiver, when it declares it) plus that object, and
/// emits `if (!ci.isCancelled()) goto body; return`, so a cancelled callback
/// skips the target body and the method answers with the `void` default.  A
/// non-cancellable `HEAD` plan keeps the plain [`weave_head`] shape byte for
/// byte.
///
/// Only a `void` target is covered: a non-void one would also need a default
/// value in the target's shape, which the interpreter path supplies.
pub fn weave_head_cancellable(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
) -> Result<Vec<u8>, String> {
    let target = prepare_target(class, target)?;
    let label = target.label();
    if crate::return_desc(&target.descriptor) != "V" {
        return Err(format!(
            "{label}: cancellable HEAD weaving is bounded to void targets; a non-void target needs a CallbackInfoReturnable and stays on the interpreter path"
        ));
    }
    let shape = callback_arguments(
        &target.class_name,
        target.is_static,
        &target.descriptor,
        &callback.descriptor,
        "HEAD",
        Injected::Cancellable,
    )?;

    let code = target.code()?;
    let scan = scan_code(&label, code.code)?;
    let slot = code.max_locals.max(argument_slots(&target)?);
    let mut pool = PoolBuilder::new(&class.pool);
    let call = cancellable_head_call(&target, callback, shape.takes_receiver, slot, &mut pool)?;
    if code.code.len() + call.bytes.len() > MAX_CODE_LENGTH {
        return Err(format!(
            "{label}: woven code would exceed the class-file limit of {MAX_CODE_LENGTH} bytes"
        ));
    }
    let max_stack = u32::from(code.max_stack)
        .checked_add(u32::from(call.stack_slots))
        .and_then(|value| value.checked_add(1))
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| format!("{label}: woven max_stack overflows u16"))?;
    let max_locals = code.max_locals.max(call.local_slots);
    let entry = cancellable_entry_locals(&target, slot)?;
    let patch = relocate(
        &label,
        &target,
        code,
        &scan,
        &[(0, call.bytes.len())],
        &call.bytes,
        max_stack,
        max_locals,
        TargetEntry::Skip,
        FrameSites::Both,
        &[SyntheticFrame {
            offset: 0,
            locals: entry,
            stack: Vec::new(),
            at_instruction: true,
        }],
        &mut pool,
    )?;

    emit(
        &target.layout,
        Some(Patch {
            method: target.method_index,
            attribute: target.attribute_index,
            code: patch,
        }),
        &pool.added,
    )
}

/// One invocation instruction a plan selects: `invokevirtual`, `invokestatic`
/// or `invokeinterface`.
struct InvokeSite {
    at: usize,
    size: usize,
    descriptor: String,
    /// Whether the call pushes a receiver (`invokevirtual`/`invokeinterface`).
    receiver: bool,
    /// `invokespecial`, which the bounded weaver reports instead of matching.
    special: bool,
}

/// The call sites of `call_site` in one method.
///
/// A selector with an empty descriptor matches every overload of the name.
/// `invokespecial` is collected rather than matched: rewriting a constructor or
/// private call is outside the bounded shape, and the caller reports it after
/// the slice has picked the sites that are actually selected.
fn invoke_candidates(
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
    call_site: &MethodRef,
) -> Result<Vec<InvokeSite>, String> {
    let mut sites = Vec::new();
    for instruction in &scan.instructions {
        let opcode = code_u1(code.code, instruction.at)?;
        if !matches!(opcode, 0xb6..=0xb9) {
            continue;
        }
        let index = code_u2(code.code, instruction.at + 1)?;
        let Some((owner, name, descriptor)) = invoke_method_ref(target.pool, index) else {
            continue;
        };
        if owner != call_site.owner
            || name != call_site.name
            || (!call_site.descriptor.is_empty() && descriptor != call_site.descriptor)
        {
            continue;
        }
        sites.push(InvokeSite {
            at: instruction.at,
            size: instruction.size,
            descriptor: descriptor.to_owned(),
            receiver: opcode != 0xb8,
            special: opcode == 0xb7,
        });
    }
    Ok(sites)
}

/// The call sites one bounded [`InvokeSelector`] names, in file order.
///
/// The slice is applied before the ordinal, so an `invokespecial` outside the
/// slice does not make a selector inside it fail.  An empty result, an
/// `invokespecial` that was selected, and an ordinal outside the matches are
/// reasons that name the selector.
fn selected_invoke_sites(
    label: &str,
    target: &Target<'_>,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
    selector: &InvokeSelector,
) -> Result<Vec<InvokeSite>, String> {
    let call_site = MethodRef {
        owner: selector.owner.clone(),
        name: selector.name.clone(),
        descriptor: selector.descriptor.clone(),
    };
    let mut sites = invoke_candidates(target, code, scan, &call_site)?;
    if sites.is_empty() {
        return Err(format!(
            "{label}: INVOKE weaving found no {}{} call site; the selector matches invokevirtual, invokestatic and invokeinterface instructions by owner, name and descriptor, and a target that declares none stays on the interpreter path",
            selector.owner, selector.name
        ));
    }
    if let Some(slice) = &selector.slice {
        let (from, to) = resolve_slice(label, target, code, scan, slice)?;
        sites.retain(|site| site.at >= from && site.at < to);
        if sites.is_empty() {
            return Err(format!(
                "{label}: INVOKE weaving found no {}{} call site inside the slice from {} to {}",
                selector.owner,
                selector.name,
                slice.from.describe(),
                slice.to.describe()
            ));
        }
    }
    if let Some(site) = sites.iter().find(|site| site.special) {
        return Err(format!(
            "{label}: INVOKE weaving cannot rewrite the invokespecial {}.{}{} call site at {}; only invokevirtual, invokestatic and invokeinterface are bounded",
            selector.owner, selector.name, site.descriptor, site.at
        ));
    }
    if let Some(ordinal) = selector.ordinal {
        let index = ordinal_index(label, "INVOKE", sites.len(), ordinal)?;
        sites = vec![sites.remove(index)];
    }
    Ok(sites)
}

/// The insertion offset a shift puts the callback at.
///
/// `delta` is a signed instruction distance: `0` names the instruction at
/// `at`, positive moves towards the end of the method and negative towards its
/// start.  A [`Shift::Before`]/[`Shift::After`] shift converts to this distance
/// before it gets here; [`ShiftBy`] supplies it directly.  The offset is an
/// instruction boundary by construction, because it is read off the scan.  A
/// shift that runs past either end of the method, and one that would put the
/// insert strictly inside an exception range (where the target's own handler
/// would catch the callback), are reasons.
fn shifted_offset(
    label: &str,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
    at: usize,
    delta: i32,
) -> Result<usize, String> {
    let index = scan
        .instructions
        .iter()
        .position(|instruction| instruction.at == at)
        .ok_or_else(|| format!("{label}: the site at {at} is not an instruction boundary"))?;
    let target = (index as i64)
        .checked_add(i64::from(delta))
        .ok_or_else(|| {
            format!(
                "{label}: shifting the insert at {at} by {delta} instruction(s) overflows the instruction index"
            )
        })?;
    if target < 0 {
        return Err(format!(
            "{label}: shifting the insert at {at} by {delta} instruction(s) runs past the method's first instruction"
        ));
    }
    let target = usize::try_from(target)
        .ok()
        .filter(|index| *index < scan.instructions.len())
        .ok_or_else(|| {
            format!(
                "{label}: shifting the insert at {at} by {delta} instruction(s) runs past the method's last instruction"
            )
        })?;
    let shifted = scan.instructions[target].at;
    for handler in &code.exception_table {
        if usize::from(handler.start_pc) < shifted && shifted < usize::from(handler.end_pc) {
            return Err(format!(
                "{label}: shifting the insert at {at} to {shifted} would put it inside the exception range [{}, {}), so the target's own handler would catch it",
                handler.start_pc, handler.end_pc
            ));
        }
    }
    Ok(shifted)
}

/// The `pop`/`pop2` opcode that removes one call argument of this descriptor.
fn pop_opcode(descriptor: &str) -> Result<u8, String> {
    let first = descriptor
        .as_bytes()
        .first()
        .copied()
        .ok_or_else(|| "empty call argument descriptor".to_owned())?;
    Ok(match first {
        b'J' | b'D' => 0x58, // pop2
        b'V' => {
            return Err(format!("call argument descriptor {descriptor:?} is void"));
        }
        _ => 0x57, // pop
    })
}

/// One cancellable call-site insert with the positions of the two branch
/// operands the layout has to patch.
struct CancellableInvoke {
    bytes: Vec<u8>,
    ifeq: usize,
    ifeq_operand: usize,
    goto: usize,
    goto_operand: usize,
    stack_slots: u16,
    local_slots: u16,
}

/// Emit the insert that runs one callback before a `void` call and, when the
/// callback cancelled, pops the call's receiver and arguments and jumps to the
/// instruction after the call.
///
/// The inserted shape is:
///
/// ```text
/// <CallbackInfo setup and callback call>
/// aload <slot>; invokevirtual CallbackInfo.isCancelled()Z
/// ifeq <the call>          (not cancelled)
/// pop/pop2 ...             (the call's arguments, then the receiver)
/// goto <after the call>
/// <the call itself>        (the original instruction, at insert end)
/// <after the call>         (the original next instruction)
/// ```
///
/// Both branch operands are patched from the stable layout, because the `goto`
/// has to land on the start of any insert in front of the original next
/// instruction, not on the instruction itself.
fn cancellable_invoke_call(
    target: &Target<'_>,
    callback: &MethodRef,
    takes_receiver: bool,
    site: &InvokeSite,
    slot: u16,
    pool: &mut PoolBuilder<'_>,
) -> Result<CancellableInvoke, String> {
    let (mut bytes, stack_slots, local_slots) =
        callback_info_call(target, callback, takes_receiver, slot, pool)?;
    let is_cancelled = pool.method_ref(&MethodRef {
        owner: CALLBACK_INFO_CLASS.to_owned(),
        name: "isCancelled".to_owned(),
        descriptor: "()Z".to_owned(),
    })?;
    bytes.push(0xb6); // invokevirtual isCancelled
    bytes.extend_from_slice(&is_cancelled.to_be_bytes());
    push_ifeq(&mut bytes);
    let ifeq = bytes.len() - 3;
    let ifeq_operand = ifeq + 1;
    for argument in crate::descriptor_arguments(&site.descriptor).iter().rev() {
        bytes.push(pop_opcode(argument)?);
    }
    if site.receiver {
        bytes.push(0x57); // pop the receiver
    }
    push_goto(&mut bytes);
    let goto = bytes.len() - 3;
    let goto_operand = goto + 1;
    Ok(CancellableInvoke {
        bytes,
        ifeq,
        ifeq_operand,
        goto,
        goto_operand,
        stack_slots,
        local_slots,
    })
}

fn push_ifeq(out: &mut Vec<u8>) {
    out.push(0x99); // ifeq
    out.extend_from_slice(&[0, 0]);
}

fn push_goto(out: &mut Vec<u8>) {
    out.push(0xa7); // goto
    out.extend_from_slice(&[0, 0]);
}

/// Insert a callback immediately before every call site a plan selects.
///
/// `target` is the woven method (the plan's `target`); `call_site` is the
/// `@At(INVOKE)` selector.  Only `invokevirtual`, `invokestatic` and
/// `invokeinterface` call sites are bounded: those are the instructions whose
/// operand stack the insert can leave untouched (non-cancellable) or pop with
/// known types (cancellable).  A selector with an empty descriptor matches
/// every overload of the name.
///
/// The callback shape is the one the interpreter validates for an INVOKE plan:
/// it mirrors the woven method's arguments, may take the woven method's
/// receiver first, may end with a `CallbackInfo` (always for `cancellable`,
/// and as a plain constructed marker otherwise), and returns `void`.  The
/// callback therefore sees the method the call site lives in, not the callee's
/// arguments.
///
/// A non-cancellable insert is stack-neutral: it is the same load-arguments and
/// `invokestatic` sequence a `HEAD` insert emits, placed before each matching
/// call.  A cancellable insert constructs [`callback_info_class`], calls the
/// callback, and - when `ci.cancel()` ran - pops the call's arguments with
/// their own widths and branches around the call.  Cancellation is bounded to
/// a `void` call site with exactly one matching instruction, because a
/// non-void call would also have to push the callee descriptor's default value.
///
/// Both shapes relocate branches, switches, the exception table and
/// `StackMapTable` exactly as [`weave_head`] does.  A cancellable insert adds
/// the two verifier frames its branches need (the call and the instruction
/// after it), derived from the method's own frames instead of guessed.
///
/// This entry point keeps every matching site, the way it always has; for
/// Mixin's `ordinal`, `slice`, `shift` and `by` see [`weave_invoke_at`],
/// [`weave_invoke_selected`] and [`weave_invoke_shifted`].
pub fn weave_invoke(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    call_site: &MethodRef,
    cancellable: bool,
) -> Result<Vec<u8>, String> {
    weave_invoke_shape(
        class,
        target,
        callback,
        &InvokeSelector {
            owner: call_site.owner.clone(),
            name: call_site.name.clone(),
            descriptor: call_site.descriptor.clone(),
            ordinal: None,
            slice: None,
        },
        None,
        cancellable,
    )
}

/// Insert a callback at the call site `ordinal` selects.
///
/// This is [`weave_invoke`] with one bounded ordinal: `None` keeps every
/// matching site (the [`weave_invoke`] shape), `Some(n)` keeps the 0-based
/// `n`-th match in file order, and a negative ordinal counts from the end
/// (`-1` is the last).  An ordinal outside the matches is a reason that names
/// the count instead of falling back to the first site.
pub fn weave_invoke_at(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    invoke_target: &MethodRef,
    ordinal: Option<i32>,
    cancellable: bool,
) -> Result<Vec<u8>, String> {
    weave_invoke_shape(
        class,
        target,
        callback,
        &InvokeSelector {
            owner: invoke_target.owner.clone(),
            name: invoke_target.name.clone(),
            descriptor: invoke_target.descriptor.clone(),
            ordinal,
            slice: None,
        },
        None,
        cancellable,
    )
}

/// Insert a callback at the call site an [`InvokeSelector`] names.
///
/// The selector is [`weave_invoke_at`] plus a [`SliceBounds`]: the slice is
/// resolved to the half-open instruction range `[from, to)`, the call sites
/// inside it are counted, and the ordinal picks one of them.  `None` as the
/// ordinal keeps every site inside the slice.
pub fn weave_invoke_selected(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    selector: &InvokeSelector,
    cancellable: bool,
) -> Result<Vec<u8>, String> {
    weave_invoke_shape(class, target, callback, selector, None, cancellable)
}

/// Insert a callback `shift.by` instruction boundaries from the selected call
/// site, in the direction `shift.direction` names.
///
/// `by == 0` is the site itself for [`Shift::Before`] and the instruction after
/// the call for [`Shift::After`].  The bound is `0..=8`, the offset is always an
/// instruction boundary, and an insert that would land strictly inside an
/// exception range is rejected because the target's own handler would catch it.
/// The callback is still inserted with [`TargetEntry::Enter`], so a branch that
/// named the shifted instruction runs it first.  A cancellable insert cannot be
/// shifted: its cancellation branch has to sit on the call to skip it.
pub fn weave_invoke_shifted(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    selector: &InvokeSelector,
    shift: InvokeShift,
    cancellable: bool,
) -> Result<Vec<u8>, String> {
    weave_invoke_shape(
        class,
        target,
        callback,
        selector,
        Some(shift.into()),
        cancellable,
    )
}

/// Insert a callback a signed [`Shift::By`] distance from the selected call
/// site: positive moves towards the end of the method, negative towards its
/// start, and `0` names the call itself.
///
/// This is Mixin's `@At(shift = Shift.BY, by = n)`.  The magnitude bound is
/// `-8..=8`, the offset is always an instruction boundary, and an insert that
/// would land strictly inside an exception range is rejected because the
/// target's own handler would catch it.  The callback is inserted with
/// [`TargetEntry::Enter`], so a branch that named the shifted instruction runs
/// it first.  A cancellable insert cannot be shifted: its cancellation branch
/// has to sit on the call to skip it.
pub fn weave_invoke_shifted_by(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    selector: &InvokeSelector,
    shift: ShiftBy,
    cancellable: bool,
) -> Result<Vec<u8>, String> {
    weave_invoke_shape(
        class,
        target,
        callback,
        selector,
        Some(AnyShift::By(shift.by)),
        cancellable,
    )
}

/// The shared body of every `INVOKE` entry point.
fn weave_invoke_shape(
    class: &ClassFile,
    target: &MethodRef,
    callback: &MethodRef,
    selector: &InvokeSelector,
    shift: Option<AnyShift>,
    cancellable: bool,
) -> Result<Vec<u8>, String> {
    let target = prepare_target(class, target)?;
    let label = target.label();
    if cancellable && shift.is_some() {
        return Err(format!(
            "{label}: a cancellable INVOKE insert cannot be shifted; its cancellation branch has to sit on the call it skips"
        ));
    }
    let shape = callback_arguments(
        &target.class_name,
        target.is_static,
        &target.descriptor,
        &callback.descriptor,
        "INVOKE",
        if cancellable {
            Injected::Cancellable
        } else {
            Injected::None
        },
    )?;
    let code = target.code()?;
    let scan = scan_code(&label, code.code)?;
    let sites = selected_invoke_sites(&label, &target, code, &scan, selector)?;
    if cancellable {
        let [site] = sites.as_slice() else {
            return Err(format!(
                "{label}: cancellable INVOKE weaving needs exactly one matching call site, but the selector matches {}",
                sites.len()
            ));
        };
        return weave_cancellable_invoke(
            &target,
            callback,
            shape.takes_receiver,
            site,
            code,
            &scan,
        );
    }

    let offsets = match shift {
        Some(shift) => {
            let delta = shift.delta(&label)?;
            sites
                .iter()
                .map(|site| {
                    if delta == 0 {
                        Ok(site.at)
                    } else {
                        shifted_offset(&label, code, &scan, site.at, delta)
                    }
                })
                .collect::<Result<Vec<_>, String>>()?
        }
        None => sites.iter().map(|site| site.at).collect(),
    };

    let mut pool = PoolBuilder::new(&class.pool);
    let call = if shape.callback_info {
        plain_callback_info_call(&target, callback, shape.takes_receiver, &mut pool)?
    } else {
        callback_call(&target, callback, shape.takes_receiver, &mut pool)?
    };
    if code.code.len() + call.bytes.len() * offsets.len() > MAX_CODE_LENGTH {
        return Err(format!(
            "{label}: woven code would exceed the class-file limit of {MAX_CODE_LENGTH} bytes"
        ));
    }
    let max_stack = u32::from(code.max_stack)
        .checked_add(u32::from(call.stack_slots))
        .and_then(|value| value.checked_add(1))
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| format!("{label}: woven max_stack overflows u16"))?;
    let max_locals = code.max_locals.max(call.local_slots);
    let insertions = offsets
        .iter()
        .map(|at| (*at, call.bytes.len()))
        .collect::<Vec<_>>();
    let patch = relocate(
        &label,
        &target,
        code,
        &scan,
        &insertions,
        &call.bytes,
        max_stack,
        max_locals,
        TargetEntry::Enter,
        FrameSites::Both,
        &[],
        &mut pool,
    )?;
    emit(
        &target.layout,
        Some(Patch {
            method: target.method_index,
            attribute: target.attribute_index,
            code: patch,
        }),
        &pool.added,
    )
}

fn weave_cancellable_invoke(
    target: &Target<'_>,
    callback: &MethodRef,
    takes_receiver: bool,
    site: &InvokeSite,
    code: &CodeLayout<'_>,
    scan: &CodeScan,
) -> Result<Vec<u8>, String> {
    let label = target.label();
    if crate::return_desc(&site.descriptor) != "V" {
        return Err(format!(
            "{label}: cancellable INVOKE weaving is bounded to void call sites; the selected call site's descriptor {} answers a value, which would also need the callee descriptor's default pushed on cancellation",
            site.descriptor
        ));
    }
    if site.at + site.size >= code.code.len() {
        return Err(format!(
            "{label}: the call site at {} has no instruction after it for the cancellation branch to reach",
            site.at
        ));
    }
    let slot = code.max_locals.max(argument_slots(target)?);
    let mut pool = PoolBuilder::new(target.pool);
    let mut call =
        cancellable_invoke_call(target, callback, takes_receiver, site, slot, &mut pool)?;
    if code.code.len() + call.bytes.len() > MAX_CODE_LENGTH {
        return Err(format!(
            "{label}: woven code would exceed the class-file limit of {MAX_CODE_LENGTH} bytes"
        ));
    }

    // The insert's length is fixed before its branch operands are known, so the
    // stable layout can be computed first and the operands patched into the
    // bytes the relocation will place at that same offset.
    let insertions = [(site.at, call.bytes.len())];
    let insertions =
        validate_insertions(&label, scan, code.code.len(), &insertions, call.bytes.len())?;
    let plan = stable_plan(&label, scan, &insertions, TargetEntry::Enter)?;
    let call_at = plan.offsets.get(&site.at).copied().ok_or_else(|| {
        format!(
            "{label}: the call site at {} is not an instruction boundary",
            site.at
        )
    })?;
    let insert_start = call_at
        .checked_sub(call.bytes.len())
        .ok_or_else(|| format!("{label}: the insert at {} does not fit the layout", site.at))?;
    let after = map_offset(&label, &plan, site.at + site.size, false)?;
    let ifeq_delta = i16::try_from(call_at as i64 - (insert_start + call.ifeq) as i64)
        .map_err(|_| format!("{label}: the cancellation branch cannot reach the call"))?;
    let goto_delta = i16::try_from(after as i64 - (insert_start + call.goto) as i64)
        .map_err(|_| format!("{label}: the cancellation branch cannot reach the continuation"))?;
    call.bytes[call.ifeq_operand..call.ifeq_operand + 2].copy_from_slice(&ifeq_delta.to_be_bytes());
    call.bytes[call.goto_operand..call.goto_operand + 2].copy_from_slice(&goto_delta.to_be_bytes());

    let before = derive_state_at(&label, target, code, scan, site.at)?;
    let after_state = derive_state_at(&label, target, code, scan, site.at + site.size)?;
    let extras = [
        SyntheticFrame {
            offset: site.at,
            locals: before.locals,
            stack: before.stack,
            at_instruction: true,
        },
        SyntheticFrame {
            offset: site.at + site.size,
            locals: after_state.locals,
            stack: after_state.stack,
            at_instruction: false,
        },
    ];
    let max_stack = u32::from(code.max_stack)
        .checked_add(u32::from(call.stack_slots))
        .and_then(|value| value.checked_add(1))
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| format!("{label}: woven max_stack overflows u16"))?;
    let max_locals = code.max_locals.max(call.local_slots);
    let patch = relocate(
        &label,
        target,
        code,
        scan,
        &[(site.at, call.bytes.len())],
        &call.bytes,
        max_stack,
        max_locals,
        TargetEntry::Enter,
        FrameSites::Both,
        &extras,
        &mut pool,
    )?;
    emit(
        &target.layout,
        Some(Patch {
            method: target.method_index,
            attribute: target.attribute_index,
            code: patch,
        }),
        &pool.added,
    )
}

/// Apply one `@At("INVOKE")` plan: the selected call site, plus the plan's
/// `ordinal`, `slice` and `shift`/`by` modifiers.
///
/// This is the plan-driven form of [`weave_invoke_selected`] and
/// [`weave_invoke_shifted`]: a plan with no modifiers keeps every matching
/// site, exactly like [`weave_invoke`].  A plan with no resolved call site is a
/// reason, because the bounded weaver never guesses one.
pub fn weave_invoke_plan(class: &ClassFile, plan: &MixinInjectionPlan) -> Result<Vec<u8>, String> {
    if plan.at != "INVOKE" {
        return Err(format!(
            "{}.{}: plan anchor is {}, not INVOKE",
            plan.target.owner, plan.target.name, plan.at
        ));
    }
    let site = plan.invoke_target.as_ref().ok_or_else(|| {
        format!(
            "{}.{}: the @At(INVOKE) plan has no resolved call site",
            plan.target.owner, plan.target.name
        )
    })?;
    let selector = InvokeSelector {
        owner: site.owner.clone(),
        name: site.name.clone(),
        descriptor: site.descriptor.clone(),
        ordinal: plan.ordinal,
        slice: plan.slice.clone(),
    };
    match plan.shift {
        Some(shift) => weave_invoke_shifted(
            class,
            &plan.target,
            &plan.callback,
            &selector,
            shift,
            plan.cancellable,
        ),
        None => weave_invoke_selected(
            class,
            &plan.target,
            &plan.callback,
            &selector,
            plan.cancellable,
        ),
    }
}

/// Convert an `@At(shift = ..., by = ...)` pair to the signed instruction
/// distance every shifted entry point takes.
///
/// `Before(by)` moves `by` instructions towards the method start, so it is
/// `-by`; `After(by)` keeps the historical "after the site" meaning, which is
/// `by + 1` boundaries from it (`After(0)` is the next instruction).
fn plan_shift_by(shift: InvokeShift) -> ShiftBy {
    match shift.direction {
        Shift::Before => ShiftBy {
            by: -i32::from(shift.by),
        },
        Shift::After => ShiftBy {
            by: i32::from(shift.by) + 1,
        },
    }
}

/// Apply every plan that targets `class`: `HEAD` plans first, then
/// `RETURN`/`TAIL` plans, then `INVOKE` plans, each group in declaration order.
///
/// Plans for other classes are skipped.  A matching plan that asks for an
/// anchor or object this file cannot produce is an error rather than a silent
/// skip: an unknown anchor, cancelling a `void` target, an `INVOKE` plan
/// without a resolved call site, an out-of-range `ordinal`/`by`, and more than
/// `MAX_WEAVE_PLANS_PER_CLASS` plans for one class.  `HEAD` uses
/// [`weave_head_selected`], `RETURN`/`TAIL` use [`weave_returns_selected`] when
/// the plan carries an `ordinal` or a `slice`, and `INVOKE` uses
/// [`weave_invoke_plan`], which applies the plan's ordinal, slice and shift.
///
/// A `RETURN`/`TAIL` plan on a non-void target references the synthetic
/// `CallbackInfoReturnable`; [`weave_class_with_extras`] reports that class
/// alongside the rewritten bytes so a jar writer can add it.
pub fn weave_class(class: &ClassFile, plans: &[MixinInjectionPlan]) -> Result<Vec<u8>, String> {
    weave_class_with_extras(class, plans).map(|(bytes, _)| bytes)
}

/// [`weave_class`], plus the synthetic classes the rewrite references.
///
/// `added` is keyed by internal class name and holds
/// [`callback_info_returnable_class`] when a non-void `RETURN`/`TAIL` insert
/// landed and [`callback_info_class`] when a cancellable `HEAD` or `INVOKE`
/// insert landed; it is empty otherwise.  The weaver cannot tell whether the
/// jar already declares a class, so [`weave_classes_with_extras`] drops the
/// duplicates.
pub fn weave_class_with_extras(
    class: &ClassFile,
    plans: &[MixinInjectionPlan],
) -> Result<(Vec<u8>, BTreeMap<String, Vec<u8>>), String> {
    weave_class_with_extras_inner(class, plans, &[])
}

/// The shared body of [`weave_class_with_extras`] and
/// [`weave_classes_with_extras_and_instance_callbacks`].
///
/// `instance_callbacks` names the callback methods that are instance methods on
/// their owner; each one is reached through a synthetic bridge class (see
/// [`bridge_instance_callbacks`]) instead of an `invokestatic` that cannot link.
fn weave_class_with_extras_inner(
    class: &ClassFile,
    plans: &[MixinInjectionPlan],
    instance_callbacks: &[MethodRef],
) -> Result<(Vec<u8>, BTreeMap<String, Vec<u8>>), String> {
    let matching = plans
        .iter()
        .filter(|plan| plan.target.owner == class.name)
        .collect::<Vec<_>>();
    if matching.len() > MAX_WEAVE_PLANS_PER_CLASS {
        return Err(format!(
            "class {} has {} matching weave plans; the bounded weaver applies at most {MAX_WEAVE_PLANS_PER_CLASS}",
            class.name,
            matching.len()
        ));
    }
    for plan in &matching {
        if !matches!(plan.at.as_str(), "HEAD" | "RETURN" | "TAIL" | "INVOKE" | "") {
            return Err(format!(
                "{}.{}: anchor {} is not one the bounded weaver applies; it supports HEAD, RETURN, TAIL, INVOKE and the empty @At of an @Overwrite",
                plan.target.owner, plan.target.name, plan.at
            ));
        }
        if plan.at.is_empty() {
            if plan.cancellable {
                return Err(format!(
                    "{}.{}: an @Overwrite replaces the whole body and is not cancellable",
                    plan.target.owner, plan.target.name
                ));
            }
            if plan.ordinal.is_some() || plan.slice.is_some() || plan.shift.is_some() {
                return Err(format!(
                    "{}.{}: an @Overwrite replaces the whole body, so ordinal, slice and shift do not apply",
                    plan.target.owner, plan.target.name
                ));
            }
            if matching.iter().any(|other| {
                other.target.name == plan.target.name
                    && other.target.descriptor == plan.target.descriptor
                    && !other.at.is_empty()
            }) {
                return Err(format!(
                    "{}.{}: an @Overwrite replaces the whole body and cannot run alongside another plan on the same target",
                    plan.target.owner, plan.target.name
                ));
            }
            continue;
        }
        // A non-void `RETURN`/`TAIL` builds the `CallbackInfoReturnable` the
        // callback cancels with; a `void` `HEAD` builds the `CallbackInfo`.
        // Every other cancellable shape still needs a runtime-owned object the
        // bounded weaver does not construct.  A cancellable `INVOKE` is checked
        // by the invoke path, which owns its `CallbackInfo` insert.
        if plan.cancellable
            && plan.at != "HEAD"
            && plan.at != "INVOKE"
            && !plan_returns_value(class, plan)
        {
            return Err(format!(
                "{}.{}: cancellable {} weaving needs a runtime-owned CallbackInfo, which the bounded weaver cannot construct",
                plan.target.owner, plan.target.name, plan.at
            ));
        }
        if plan.cancellable && plan.at == "HEAD" && plan_returns_value_for_head(class, plan) {
            return Err(format!(
                "{}.{}: cancellable HEAD weaving of a non-void target needs the descriptor's default value in the target's shape; only void targets are bounded",
                plan.target.owner, plan.target.name
            ));
        }
    }

    let mut resolved = matching
        .iter()
        .map(|plan| (*plan).clone())
        .collect::<Vec<_>>();
    let bridges = bridge_instance_callbacks(class, &mut resolved, instance_callbacks)?;

    let mut current = class.write()?;
    let mut needs_returnable = false;
    // A plain `CallbackInfo` argument means the insert constructs the synthetic
    // class, cancellation aside: a non-cancellable callback still receives one.
    let mut needs_callback_info = resolved.iter().any(|plan| {
        crate::descriptor_arguments(&plan.callback.descriptor)
            .last()
            .is_some_and(|argument| argument == CALLBACK_INFO_DESC)
    });
    for plan in resolved.iter().filter(|plan| plan.at.is_empty()) {
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        current = weave_overwrite(&next, &plan.target, &plan.callback)?;
    }
    for plan in resolved.iter().filter(|plan| plan.at == "HEAD") {
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        let shift = plan.shift.map(plan_shift_by);
        if plan.cancellable {
            if shift.is_some() {
                return Err(format!(
                    "{}.{}: a cancellable HEAD insert cannot be shifted; its cancellation branch has to stay at the method entry",
                    plan.target.owner, plan.target.name
                ));
            }
            validate_head_modifiers(
                &next,
                &plan.target,
                &AnchorRequest {
                    site: AnchorSite::Head,
                    ordinal: plan.ordinal,
                    slice: plan.slice.clone(),
                    shift,
                    redirect: false,
                    cancellable: true,
                },
            )?;
            needs_callback_info = true;
            if let Some(ordinal) = plan.ordinal {
                ordinal_index(
                    &format!("{}.{}", plan.target.owner, plan.target.name),
                    "HEAD",
                    1,
                    ordinal,
                )?;
            }
            current = weave_head_cancellable(&next, &plan.target, &plan.callback)?;
        } else {
            validate_head_modifiers(
                &next,
                &plan.target,
                &AnchorRequest {
                    site: AnchorSite::Head,
                    ordinal: plan.ordinal,
                    slice: plan.slice.clone(),
                    shift,
                    redirect: false,
                    cancellable: false,
                },
            )?;
            current = weave_head_shape(&next, &plan.target, &plan.callback, shift)?;
        }
    }
    for plan in resolved
        .iter()
        .filter(|plan| plan.at == "RETURN" || plan.at == "TAIL")
    {
        let mode = if plan.at == "TAIL" {
            ReturnMode::Tail
        } else {
            ReturnMode::Return
        };
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        let returns_value = plan_returns_value(&next, plan);
        needs_returnable |= returns_value;
        let selector = ReturnSelector {
            mode,
            ordinal: plan.ordinal,
            slice: plan.slice.clone(),
        };
        current = if let Some(shift) = plan.shift.map(plan_shift_by) {
            weave_returns_shifted(
                &next,
                &plan.target,
                &plan.callback,
                returns_value,
                &selector,
                shift,
            )?
        } else if plan.ordinal.is_some() || plan.slice.is_some() {
            weave_returns_selected(
                &next,
                &plan.target,
                &plan.callback,
                returns_value,
                &selector,
            )?
        } else {
            weave_return(&next, &plan.target, &plan.callback, mode)?
        };
    }
    for plan in resolved.iter().filter(|plan| plan.at == "INVOKE") {
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        // A cancellable INVOKE constructs the same runtime-owned `CallbackInfo`
        // a cancellable HEAD does, so the synthetic class is reported too.
        needs_callback_info |= plan.cancellable;
        current = weave_invoke_plan(&next, plan)?;
    }
    let mut added = BTreeMap::new();
    if needs_returnable {
        added.insert(
            CALLBACK_INFO_RETURNABLE_CLASS.to_owned(),
            callback_info_returnable_class()?,
        );
    }
    if needs_callback_info {
        added.insert(CALLBACK_INFO_CLASS.to_owned(), callback_info_class()?);
    }
    added.extend(bridges);
    Ok((current, added))
}

/// One `invokevirtual` bridge method and its installer, as the pool indices
/// and code bytes [`bridge_class`] writes.
struct BridgeMethod {
    /// The static field holding the callback owner's instance.  The callback
    /// method uses the same name: fields and methods are separate namespaces.
    name: u16,
    descriptor: u16,
    install_name: u16,
    install_descriptor: u16,
    callback_body: Vec<u8>,
    callback_max_stack: u16,
    callback_max_locals: u16,
    install_body: Vec<u8>,
}

/// Whether `callback` is one of the instance callbacks the caller named.
fn is_instance_callback(instance_callbacks: &[MethodRef], callback: &MethodRef) -> bool {
    instance_callbacks
        .iter()
        .any(|candidate| candidate == callback)
}

/// The name of the synthetic bridge that reaches `callback_owner`'s instance
/// callbacks from `target`.
///
/// The bridge stays in the callback owner's package, so a package-visible
/// callback method is invocable from it; the target is hashed into the name so
/// two targets that share one mixin class get their own bridge (and therefore
/// their own instance) instead of colliding in the jar.
fn bridge_class_name(callback_owner: &str, target: &str) -> String {
    format!(
        "{callback_owner}$$HorizonBridge${:08x}",
        fnv1a_32(target.as_bytes())
    )
}

/// FNV-1a, only to make the bridge name deterministic and target-specific.
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Replace every instance-callback plan's callback with a static method on a
/// synthetic bridge class, and return those bridge classes.
///
/// The weaver only emits `invokestatic`, which cannot link an instance method.
/// A bridge is the smallest correct indirection: one synthetic class per
/// (target, callback owner) pair, holding one static field per distinct
/// callback method, plus
///
/// - `callback<i>(<callback descriptor>)`, which loads the field, loads the
///   callback's own arguments and `invokevirtual`s it - exactly the descriptor
///   the woven insert already knows how to call;
/// - `install<i>(L<callback owner>;)V`, the static setter the host calls at
///   load to give the bridge the instance the woven code runs callbacks on.
///
/// The woven class therefore keeps calling `invokestatic`; only the callee is
/// the bridge.  A bridge field starts `null`, so a host that never installs the
/// instance fails loudly with a `NullPointerException` rather than running some
/// other object's state.  The bridge is placed in the callback owner's package
/// so a package-visible callback is reachable; a `private` callback is rejected
/// by [`weave_classes_with_extras_and_instance_callbacks`] because no separate
/// class can invoke it.
fn bridge_instance_callbacks(
    class: &ClassFile,
    plans: &mut [MixinInjectionPlan],
    instance_callbacks: &[MethodRef],
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut by_owner: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    for plan in plans.iter() {
        if is_instance_callback(instance_callbacks, &plan.callback) {
            by_owner
                .entry(plan.callback.owner.clone())
                .or_default()
                .insert((plan.callback.name.clone(), plan.callback.descriptor.clone()));
        }
    }
    let (added, bridges) = build_callback_bridges(class, &by_owner)?;
    for plan in plans.iter_mut() {
        let key = (
            plan.callback.owner.clone(),
            plan.callback.name.clone(),
            plan.callback.descriptor.clone(),
        );
        if let Some(bridge) = bridges.get(&key) {
            plan.callback = bridge.clone();
        }
    }
    Ok(added)
}

/// One synthetic bridge per callback owner, holding one static field per
/// distinct callback method.
fn build_callback_bridges(
    class: &ClassFile,
    by_owner: &BTreeMap<String, BTreeSet<(String, String)>>,
) -> Result<
    (
        BTreeMap<String, Vec<u8>>,
        BTreeMap<(String, String, String), MethodRef>,
    ),
    String,
> {
    let mut added = BTreeMap::new();
    let mut bridges = BTreeMap::new();
    for (owner, methods) in by_owner {
        let bridge_name = bridge_class_name(owner, &class.name);
        if bridge_name.len() > u16::MAX as usize {
            return Err(format!(
                "{owner}: the synthetic bridge name for {} is longer than the class-file name limit",
                class.name
            ));
        }
        added.insert(
            bridge_name.clone(),
            bridge_class(&bridge_name, owner, methods)?,
        );
        for (index, (name, descriptor)) in methods.iter().enumerate() {
            bridges.insert(
                (owner.clone(), name.clone(), descriptor.clone()),
                MethodRef {
                    owner: bridge_name.clone(),
                    name: format!("callback{index}"),
                    descriptor: descriptor.clone(),
                },
            );
        }
    }
    Ok((added, bridges))
}

/// The class file [`bridge_instance_callbacks`] adds beside a woven target.
fn bridge_class(
    bridge_name: &str,
    callback_owner: &str,
    methods: &BTreeSet<(String, String)>,
) -> Result<Vec<u8>, String> {
    if methods.is_empty() {
        return Err(format!(
            "{bridge_name}: a callback bridge needs at least one callback method"
        ));
    }
    let owner_descriptor = format!("L{callback_owner};");
    let install_descriptor_text = format!("({owner_descriptor})V");
    let mut pool: Vec<Cp> = vec![Cp::Empty];
    let this_class = pool_class(&mut pool, bridge_name);
    let super_class = pool_class(&mut pool, "java/lang/Object");
    let code_name = pool_utf8(&mut pool, "Code");
    let owner_descriptor = pool_utf8(&mut pool, &owner_descriptor);
    let install_descriptor = pool_utf8(&mut pool, &install_descriptor_text);
    let mut bridge_methods = Vec::with_capacity(methods.len());
    for (index, (name, descriptor_text)) in methods.iter().enumerate() {
        if !crate::valid_method_descriptor(descriptor_text) {
            return Err(format!(
                "{bridge_name}: callback descriptor {descriptor_text:?} is not a method descriptor"
            ));
        }
        let return_descriptor = crate::return_desc(descriptor_text);
        let method_name = pool_utf8(&mut pool, &format!("callback{index}"));
        let install_name = pool_utf8(&mut pool, &format!("install{index}"));
        let callback_name = pool_utf8(&mut pool, name);
        let descriptor = pool_utf8(&mut pool, descriptor_text);
        let field = pool_field_ref(&mut pool, bridge_name, method_name, owner_descriptor);
        let call = pool_method_ref(&mut pool, callback_owner, callback_name, descriptor);
        let mut callback_body = vec![0xb2]; // getstatic <field>
        callback_body.extend_from_slice(&field.to_be_bytes());
        let mut slot = 0u16;
        for argument in crate::descriptor_arguments(descriptor_text) {
            let (opcode, width) = load_opcode(&argument)?;
            local_opcode(&mut callback_body, opcode, slot);
            slot = slot.checked_add(width).ok_or("local slot overflow")?;
        }
        callback_body.push(0xb6); // invokevirtual <callback>
        callback_body.extend_from_slice(&call.to_be_bytes());
        callback_body.push(return_opcode(return_descriptor)?);
        let mut install_body = vec![0x2a, 0xb3]; // aload_0; putstatic <field>
        install_body.extend_from_slice(&field.to_be_bytes());
        install_body.push(0xb1); // return
        bridge_methods.push(BridgeMethod {
            name: method_name,
            descriptor,
            install_name,
            install_descriptor,
            callback_body,
            // The receiver and every argument, or the return value when it is
            // wider (a zero-argument callback returning a long still needs two
            // stack slots after the call).
            callback_max_stack: slot.saturating_add(1).max(match return_descriptor {
                "J" | "D" => 2,
                "V" => 0,
                _ => 1,
            }),
            callback_max_locals: slot,
            install_body,
        });
    }

    let mut out = Vec::new();
    push_u4(&mut out, 0xCAFE_BABE);
    push_u2(&mut out, 0); // minor_version
    push_u2(&mut out, SYNTHETIC_CLASS_MAJOR);
    push_u2(&mut out, pool.len() as u16);
    for constant in &pool[1..] {
        encode_constant(&mut out, constant)?;
    }
    push_u2(&mut out, ACC_PUBLIC | ACC_SUPER);
    push_u2(&mut out, this_class);
    push_u2(&mut out, super_class);
    push_u2(&mut out, 0); // interfaces
    push_u2(&mut out, bridge_methods.len() as u16); // fields
    for method in &bridge_methods {
        push_synthetic_field(
            &mut out,
            ACC_PRIVATE | ACC_STATIC,
            method.name,
            owner_descriptor,
        );
    }
    push_u2(&mut out, (bridge_methods.len() * 2) as u16); // methods
    for method in &bridge_methods {
        push_synthetic_method(
            &mut out,
            code_name,
            ACC_PUBLIC | ACC_STATIC,
            method.install_name,
            method.install_descriptor,
            SyntheticCode {
                max_stack: 1,
                max_locals: 1,
                bytes: &method.install_body,
            },
        );
    }
    for method in &bridge_methods {
        push_synthetic_method(
            &mut out,
            code_name,
            ACC_PUBLIC | ACC_STATIC,
            method.name,
            method.descriptor,
            SyntheticCode {
                max_stack: method.callback_max_stack,
                max_locals: method.callback_max_locals,
                bytes: &method.callback_body,
            },
        );
    }
    push_u2(&mut out, 0); // class attributes
    Ok(out)
}

/// Whether a `HEAD` plan targets a method whose descriptor returns a value.
fn plan_returns_value_for_head(class: &ClassFile, plan: &MixinInjectionPlan) -> bool {
    class
        .method(&plan.target.name, &plan.target.descriptor)
        .is_some_and(|method| crate::return_desc(&method.descriptor) != "V")
}

/// Whether one `RETURN`/`TAIL` plan targets a method with a value to carry.
///
/// A descriptor-less plan never resolves here; the weaver reports that
/// ambiguity itself, so the answer is `false` and the plan keeps the `void`
/// shape for the cancellable check.
fn plan_returns_value(class: &ClassFile, plan: &MixinInjectionPlan) -> bool {
    matches!(plan.at.as_str(), "RETURN" | "TAIL")
        && class
            .method(&plan.target.name, &plan.target.descriptor)
            .is_some_and(|method| crate::return_desc(&method.descriptor) != "V")
}

/// Apply the `HEAD` plans that target `class`, in plan order.
///
/// Only plans whose [`MixinInjectionPlan::at`] is `"HEAD"`, whose target owner
/// is this class and whose target method exists are applied; plans for other
/// classes and every other anchor are left to the interpreter.  A plan's
/// `ordinal` follows [`weave_head_selected`], so `0`/`-1` name the single
/// `HEAD` site and any other value is a reason.  Cancellable plans are skipped:
/// this entry point returns only bytes, so it cannot report the synthetic
/// `CallbackInfo` they reference - [`weave_class_with_extras`] can.
///
/// [`weave_class`] is the whole-class entry point: it also applies `RETURN`,
/// `TAIL` and `INVOKE` plans, and rejects anchors and cancellable plans it
/// cannot weave instead of skipping them.
///
/// Not supported here, and left to the interpreter: `RETURN`, `TAIL` and
/// `INVOKE` anchors (the last is [`weave_invoke_plan`]), cancellation, and
/// instance callbacks (this entry point has no bridge class to add).  A
/// trailing plain `CallbackInfo` is accepted, but the bytes reference
/// [`callback_info_class`], which a caller that writes a jar has to add itself.
/// Errors from a matching plan (ambiguous target descriptor, callback mismatch,
/// a malformed condy pool, a `wide` prefix that is not a local-variable
/// instruction, a conditional branch the split cannot express) propagate rather
/// than being silently skipped.
///
/// Every insert lands at offset 0, so when several plans match one method the
/// last plan applied is the first callback to run.  Callers that need Mixin's
/// declaration order should pass the plans in reverse.
pub fn weave_plans(class: &ClassFile, plans: &[MixinInjectionPlan]) -> Result<Vec<u8>, String> {
    let mut current = class.write()?;
    for plan in plans {
        if plan.at != "HEAD" || plan.cancellable || plan.target.owner != class.name {
            continue;
        }
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        current = weave_head_selected(&next, &plan.target, &plan.callback, plan.ordinal)?;
    }
    Ok(current)
}

/// A woven class set: the rewritten input classes plus the synthetic classes
/// their bytecode references.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WovenClasses {
    /// Every input class, rewritten or re-encoded unchanged, keyed like the
    /// input set.
    pub classes: BTreeMap<String, Vec<u8>>,
    /// Synthetic classes to add beside them, keyed by internal class name.
    /// Empty unless a non-void `RETURN`/`TAIL` insert landed and the input set
    /// does not already declare the class.
    pub added: BTreeMap<String, Vec<u8>>,
}

/// Weave a set of parsed classes, grouped by the class each plan targets.
///
/// Classes with matching plans go through [`weave_class`]; every other class is
/// re-encoded unchanged by [`ClassFile::write`], so the result is a complete
/// replacement set keyed like `classes`.  A plan that targets a class absent
/// from the set is an error rather than a silent skip, and the per-class plan
/// bound is `MAX_WEAVE_PLANS_PER_CLASS`.
///
/// The result drops the class set, so a caller that also writes a jar wants
/// [`weave_classes_with_extras`].
pub fn weave_classes(
    classes: &BTreeMap<String, ClassFile>,
    plans: &[MixinInjectionPlan],
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    weave_classes_with_extras(classes, plans).map(|woven| woven.classes)
}

/// [`weave_classes`], plus the synthetic classes the rewrites reference.
///
/// [`WovenClasses::added`] holds `CallbackInfoReturnable` when a non-void
/// `RETURN`/`TAIL` insert landed and `CallbackInfo` when any insert passes one,
/// unless the input already declares the class - a jar that ships its own copy
/// keeps it.  It also holds one bridge class per (target, callback owner) pair
/// whose callbacks are instance methods; those are detected from `classes`
/// here, because the class set knows which callback methods are static.
pub fn weave_classes_with_extras(
    classes: &BTreeMap<String, ClassFile>,
    plans: &[MixinInjectionPlan],
) -> Result<WovenClasses, String> {
    let instance_callbacks = detect_instance_callbacks(classes, plans);
    weave_classes_with_extras_and_instance_callbacks(classes, plans, &instance_callbacks)
}

/// The instance methods among the plans' callbacks that `classes` declares.
fn detect_instance_callbacks(
    classes: &BTreeMap<String, ClassFile>,
    plans: &[MixinInjectionPlan],
) -> Vec<MethodRef> {
    let mut seen = BTreeSet::new();
    let mut instance = Vec::new();
    for plan in plans {
        if let Some(method) = classes
            .get(&plan.callback.owner)
            .and_then(|class| class.method(&plan.callback.name, &plan.callback.descriptor))
            && !method.is_static()
        {
            let key = (
                plan.callback.owner.clone(),
                plan.callback.name.clone(),
                plan.callback.descriptor.clone(),
            );
            if seen.insert(key) {
                instance.push(plan.callback.clone());
            }
        }
    }
    instance
}

/// [`weave_classes_with_extras`] with the instance callbacks named explicitly.
///
/// This is the entry point for a caller that has a *partial* class set (for
/// example the load-time weave, which reads callback staticness from the jar
/// but weaves one target class at a time): pass every plan callback that is an
/// instance method.  Each one is reached through a synthetic bridge class the
/// rewrite adds, so the woven code keeps its `invokestatic`:
///
/// ```text
/// getstatic bridge.callback<i> : L<callback owner>;
/// <load the callback's arguments>
/// invokevirtual <callback owner>.<callback name><descriptor>
/// ```
///
/// The host installs the instance the bridge delegates to with the bridge's
/// `install<i>(L<callback owner>;)V` static method.  A `private` callback is a
/// reason: a separate class cannot invoke it, and the bounded weave does not
/// rewrite the callback owner to add an accessor.  This entry point can only
/// check that when `classes` holds the callback owner; a caller with a partial
/// class set must leave a private instance callback out of
/// `instance_callbacks` and report it itself.
pub fn weave_classes_with_extras_and_instance_callbacks(
    classes: &BTreeMap<String, ClassFile>,
    plans: &[MixinInjectionPlan],
    instance_callbacks: &[MethodRef],
) -> Result<WovenClasses, String> {
    for callback in instance_callbacks {
        if let Some(method) = classes
            .get(&callback.owner)
            .and_then(|class| class.method(&callback.name, &callback.descriptor))
            && method.access & ACC_PRIVATE != 0
        {
            return Err(format!(
                "Mixin callback {}.{}{} is private; a synthetic bridge class cannot invoke it, so the bounded weave needs it public or package-visible",
                callback.owner, callback.name, callback.descriptor
            ));
        }
    }
    let mut by_owner: BTreeMap<&str, Vec<&MixinInjectionPlan>> = BTreeMap::new();
    for plan in plans {
        by_owner
            .entry(plan.target.owner.as_str())
            .or_default()
            .push(plan);
    }
    for owner in by_owner.keys() {
        if !classes.values().any(|class| class.name == *owner) {
            return Err(format!(
                "weave plan targets class {owner}, which is not in the class set"
            ));
        }
    }

    let mut woven = WovenClasses::default();
    for (key, class) in classes {
        match by_owner.get(class.name.as_str()) {
            Some(class_plans) => {
                let owned = class_plans
                    .iter()
                    .map(|plan| (*plan).clone())
                    .collect::<Vec<_>>();
                let (bytes, added) =
                    weave_class_with_extras_inner(class, &owned, instance_callbacks)?;
                for (name, bytes) in added {
                    woven.added.entry(name).or_insert(bytes);
                }
                woven.classes.insert(key.clone(), bytes);
            }
            None => {
                woven.classes.insert(key.clone(), class.write()?);
            }
        }
    }
    let declared = classes
        .values()
        .map(|class| class.name.clone())
        .collect::<BTreeSet<_>>();
    woven.added.retain(|name, _| !declared.contains(name));
    Ok(woven)
}

/// Every callback among the extended plan families that `classes` declares as
/// an instance method.
pub fn detect_extended_instance_callbacks(
    classes: &BTreeMap<String, ClassFile>,
    injections: &[MixinInjectionPlan],
    anchors: &[MixinAnchorPlan],
    modify_args: &[MixinModifyArgPlan],
    modify_variables: &[MixinModifyVariablePlan],
) -> Vec<MethodRef> {
    let mut seen = BTreeSet::new();
    let mut instance = Vec::new();
    let mut consider = |callback: &MethodRef| {
        if let Some(method) = classes
            .get(&callback.owner)
            .and_then(|class| class.method(&callback.name, &callback.descriptor))
            && !method.is_static()
        {
            let key = (
                callback.owner.clone(),
                callback.name.clone(),
                callback.descriptor.clone(),
            );
            if seen.insert(key) {
                instance.push(callback.clone());
            }
        }
    };
    for plan in injections {
        consider(&plan.callback);
    }
    for plan in anchors {
        consider(&plan.callback);
    }
    for plan in modify_args {
        consider(&plan.callback);
    }
    for plan in modify_variables {
        consider(&plan.callback);
    }
    instance
}

/// Replace every instance callback in every extended plan family with a static
/// bridge method, and return the bridge classes.
fn bridge_extended_callbacks(
    class: &ClassFile,
    injections: &mut [MixinInjectionPlan],
    anchors: &mut [MixinAnchorPlan],
    modify_args: &mut [MixinModifyArgPlan],
    modify_variables: &mut [MixinModifyVariablePlan],
    instance_callbacks: &[MethodRef],
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut by_owner: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    let mut consider = |callback: &MethodRef| {
        if is_instance_callback(instance_callbacks, callback) {
            by_owner
                .entry(callback.owner.clone())
                .or_default()
                .insert((callback.name.clone(), callback.descriptor.clone()));
        }
    };
    for plan in injections.iter() {
        consider(&plan.callback);
    }
    for plan in anchors.iter() {
        consider(&plan.callback);
    }
    for plan in modify_args.iter() {
        consider(&plan.callback);
    }
    for plan in modify_variables.iter() {
        consider(&plan.callback);
    }
    let (added, bridges) = build_callback_bridges(class, &by_owner)?;
    let bridged = |callback: &mut MethodRef| {
        let key = (
            callback.owner.clone(),
            callback.name.clone(),
            callback.descriptor.clone(),
        );
        if let Some(bridge) = bridges.get(&key) {
            *callback = bridge.clone();
        }
    };
    for plan in injections.iter_mut() {
        bridged(&mut plan.callback);
    }
    for plan in anchors.iter_mut() {
        bridged(&mut plan.callback);
    }
    for plan in modify_args.iter_mut() {
        bridged(&mut plan.callback);
    }
    for plan in modify_variables.iter_mut() {
        bridged(&mut plan.callback);
    }
    Ok(added)
}

/// Weave one class with every extended plan family.
///
/// `@ModifyVariable` runs first, then `@ModifyArg` and the anchor plans, then
/// the injection family through [`weave_class_with_extras`] (so `@Overwrite`,
/// `HEAD` shift/slice and `RETURN`/`TAIL` shift land).  The count-sensitive
/// families therefore see the original local accesses and call sites, and each
/// rewrite re-parses the class so every selector sees the current bytecode.
/// Synthetic classes the rewrite references are returned in the map, including
/// `CallbackInfo`, `CallbackInfoReturnable` and one bridge per (target, callback
/// owner) whose callbacks are instance methods.
pub fn weave_class_extended(
    class: &ClassFile,
    injections: &[MixinInjectionPlan],
    anchors: &[MixinAnchorPlan],
    modify_args: &[MixinModifyArgPlan],
    modify_variables: &[MixinModifyVariablePlan],
) -> Result<(Vec<u8>, BTreeMap<String, Vec<u8>>), String> {
    weave_class_extended_with_instance_callbacks(
        class,
        injections,
        anchors,
        modify_args,
        modify_variables,
        &[],
    )
}

/// [`weave_class_extended`] with the instance callbacks named explicitly.
pub fn weave_class_extended_with_instance_callbacks(
    class: &ClassFile,
    injections: &[MixinInjectionPlan],
    anchors: &[MixinAnchorPlan],
    modify_args: &[MixinModifyArgPlan],
    modify_variables: &[MixinModifyVariablePlan],
    instance_callbacks: &[MethodRef],
) -> Result<(Vec<u8>, BTreeMap<String, Vec<u8>>), String> {
    let mut injections = injections
        .iter()
        .filter(|plan| plan.target.owner == class.name)
        .cloned()
        .collect::<Vec<_>>();
    let mut anchors = anchors
        .iter()
        .filter(|plan| plan.target.owner == class.name)
        .cloned()
        .collect::<Vec<_>>();
    let mut modify_args = modify_args
        .iter()
        .filter(|plan| plan.target.owner == class.name)
        .cloned()
        .collect::<Vec<_>>();
    let mut modify_variables = modify_variables
        .iter()
        .filter(|plan| plan.target.owner == class.name)
        .cloned()
        .collect::<Vec<_>>();
    let total = injections.len() + anchors.len() + modify_args.len() + modify_variables.len();
    if total > MAX_WEAVE_PLANS_PER_CLASS {
        return Err(format!(
            "class {} has {total} matching extended weave plans; the bounded weaver applies at most {MAX_WEAVE_PLANS_PER_CLASS}",
            class.name
        ));
    }
    for overwrite in injections.iter().filter(|plan| plan.at.is_empty()) {
        let same_target = |target: &MethodRef| {
            target.name == overwrite.target.name && target.descriptor == overwrite.target.descriptor
        };
        if anchors.iter().any(|plan| same_target(&plan.target))
            || modify_args.iter().any(|plan| same_target(&plan.target))
            || modify_variables
                .iter()
                .any(|plan| same_target(&plan.target))
        {
            return Err(format!(
                "{}.{}: an @Overwrite replaces the whole body and cannot run alongside another plan family on the same target",
                overwrite.target.owner, overwrite.target.name
            ));
        }
    }
    let bridges = bridge_extended_callbacks(
        class,
        &mut injections,
        &mut anchors,
        &mut modify_args,
        &mut modify_variables,
        instance_callbacks,
    )?;

    // Count-sensitive families run first, before any insert adds a local load
    // or a call site they would count.  Selector-based anchors then see the
    // rewritten code, and the injection family runs last because its
    // HEAD/RETURN/TAIL/INVOKE selectors do not depend on bytecode offsets.
    let mut current = class.write()?;
    for plan in &modify_variables {
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        current = weave_modify_variable(&next, plan)?;
    }
    for plan in &modify_args {
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        current = weave_modify_arg(&next, plan)?;
    }
    for plan in &anchors {
        let next = ClassFile::parse(&current)
            .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
        current = weave_anchor_plan(&next, plan)?;
    }
    let current_class = ClassFile::parse(&current)
        .map_err(|error| format!("rewritten class did not parse again: {error}"))?;
    let (current, mut added) = weave_class_with_extras_inner(&current_class, &injections, &[])?;
    let needs_callback_info = anchors.iter().any(|plan| {
        crate::descriptor_arguments(&plan.callback.descriptor)
            .last()
            .is_some_and(|argument| argument == CALLBACK_INFO_DESC)
    });
    if needs_callback_info {
        added
            .entry(CALLBACK_INFO_CLASS.to_owned())
            .or_insert(callback_info_class()?);
    }
    added.extend(bridges);
    Ok((current, added))
}

/// Weave every class in a set with all extended plan families.
///
/// A plan that targets a class absent from the set is an error, the way
/// [`weave_classes_with_extras`] reports it.  Instance callbacks are detected
/// from the class set.
pub fn weave_extended_classes(
    classes: &BTreeMap<String, ClassFile>,
    injections: &[MixinInjectionPlan],
    anchors: &[MixinAnchorPlan],
    modify_args: &[MixinModifyArgPlan],
    modify_variables: &[MixinModifyVariablePlan],
) -> Result<WovenClasses, String> {
    let instance_callbacks = detect_extended_instance_callbacks(
        classes,
        injections,
        anchors,
        modify_args,
        modify_variables,
    );
    weave_extended_classes_with_instance_callbacks(
        classes,
        injections,
        anchors,
        modify_args,
        modify_variables,
        &instance_callbacks,
    )
}

/// [`weave_extended_classes`] with the instance callbacks named explicitly.
pub fn weave_extended_classes_with_instance_callbacks(
    classes: &BTreeMap<String, ClassFile>,
    injections: &[MixinInjectionPlan],
    anchors: &[MixinAnchorPlan],
    modify_args: &[MixinModifyArgPlan],
    modify_variables: &[MixinModifyVariablePlan],
    instance_callbacks: &[MethodRef],
) -> Result<WovenClasses, String> {
    for callback in instance_callbacks {
        if let Some(method) = classes
            .get(&callback.owner)
            .and_then(|class| class.method(&callback.name, &callback.descriptor))
            && method.access & ACC_PRIVATE != 0
        {
            return Err(format!(
                "Mixin callback {}.{}{} is private; a synthetic bridge class cannot invoke it, so the bounded weave needs it public or package-visible",
                callback.owner, callback.name, callback.descriptor
            ));
        }
    }
    let mut owners = BTreeSet::new();
    for plan in injections {
        owners.insert(plan.target.owner.as_str());
    }
    for plan in anchors {
        owners.insert(plan.target.owner.as_str());
    }
    for plan in modify_args {
        owners.insert(plan.target.owner.as_str());
    }
    for plan in modify_variables {
        owners.insert(plan.target.owner.as_str());
    }
    for owner in owners {
        if !classes.values().any(|class| class.name == owner) {
            return Err(format!(
                "weave plan targets class {owner}, which is not in the class set"
            ));
        }
    }

    let mut woven = WovenClasses::default();
    for (key, class) in classes {
        let owned = injections
            .iter()
            .any(|plan| plan.target.owner == class.name)
            || anchors.iter().any(|plan| plan.target.owner == class.name)
            || modify_args
                .iter()
                .any(|plan| plan.target.owner == class.name)
            || modify_variables
                .iter()
                .any(|plan| plan.target.owner == class.name);
        if owned {
            let (bytes, added) = weave_class_extended_with_instance_callbacks(
                class,
                injections,
                anchors,
                modify_args,
                modify_variables,
                instance_callbacks,
            )?;
            for (name, bytes) in added {
                woven.added.entry(name).or_insert(bytes);
            }
            woven.classes.insert(key.clone(), bytes);
        } else {
            woven.classes.insert(key.clone(), class.write()?);
        }
    }
    let declared = classes
        .values()
        .map(|class| class.name.clone())
        .collect::<BTreeSet<_>>();
    woven.added.retain(|name, _| !declared.contains(name));
    Ok(woven)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u2(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn u4(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn utf8(out: &mut Vec<u8>, value: &str) {
        out.push(1);
        u2(out, value.len() as u16);
        out.extend_from_slice(value.as_bytes());
    }

    /// A minimal class with one method.  Pool: 1 `<class>`, 2 Class 1,
    /// 3 `java/lang/Object`, 4 Class 3, 5 `<method>`, 6 descriptor, 7 `Code`.
    fn named_fixture(
        class_name: &str,
        method_name: &str,
        access: u16,
        descriptor: &str,
        max_locals: u16,
        code: &[u8],
        exception_table: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        u4(&mut out, 0xCAFE_BABE);
        u2(&mut out, 0);
        u2(&mut out, 52);
        u2(&mut out, 8);
        utf8(&mut out, class_name);
        out.extend_from_slice(&[7, 0, 1]);
        utf8(&mut out, "java/lang/Object");
        out.extend_from_slice(&[7, 0, 3]);
        utf8(&mut out, method_name);
        utf8(&mut out, descriptor);
        utf8(&mut out, "Code");
        u2(&mut out, 0x0021); // access
        u2(&mut out, 2); // this
        u2(&mut out, 4); // super
        u2(&mut out, 0); // interfaces
        u2(&mut out, 0); // fields
        u2(&mut out, 1); // methods
        u2(&mut out, access);
        u2(&mut out, 5);
        u2(&mut out, 6);
        u2(&mut out, 1); // attributes
        u2(&mut out, 7);
        let mut body = Vec::new();
        u2(&mut body, 2); // max_stack
        u2(&mut body, max_locals);
        u4(&mut body, code.len() as u32);
        body.extend_from_slice(code);
        u2(&mut body, (exception_table.len() / 8) as u16);
        body.extend_from_slice(exception_table);
        u2(&mut body, 0); // Code attributes
        u4(&mut out, body.len() as u32);
        out.extend_from_slice(&body);
        u2(&mut out, 0); // class attributes
        out
    }

    /// A minimal `demo/T` with one method named `m`.
    fn fixture(
        access: u16,
        descriptor: &str,
        max_locals: u16,
        code: &[u8],
        exception_table: &[u8],
    ) -> Vec<u8> {
        named_fixture(
            "demo/T",
            "m",
            access,
            descriptor,
            max_locals,
            code,
            exception_table,
        )
    }

    fn callback(descriptor: &str) -> MethodRef {
        MethodRef {
            owner: "demo/Cb".into(),
            name: "cb".into(),
            descriptor: descriptor.into(),
        }
    }

    /// A `demo/T.m(II)I` whose `Code` carries a `LineNumberTable`, a
    /// `LocalVariableTable` and a one-frame `StackMapTable`:
    ///
    /// ```text
    /// 0: iload_1
    /// 1: ifne +4      -> 5
    /// 4: ireturn
    /// 5: iload_2
    /// 6: ireturn
    /// ```
    ///
    /// Pool: 1 `demo/T`, 2 Class 1, 3 `java/lang/Object`, 4 Class 3, 5 `m`,
    /// 6 `(II)I`, 7 `Code`, 8 `LineNumberTable`, 9 `LocalVariableTable`,
    /// 10 `StackMapTable`.
    fn frames_fixture() -> Vec<u8> {
        let mut out = Vec::new();
        u4(&mut out, 0xCAFE_BABE);
        u2(&mut out, 0);
        u2(&mut out, 52);
        u2(&mut out, 11);
        utf8(&mut out, "demo/T");
        out.extend_from_slice(&[7, 0, 1]);
        utf8(&mut out, "java/lang/Object");
        out.extend_from_slice(&[7, 0, 3]);
        utf8(&mut out, "m");
        utf8(&mut out, "(II)I");
        utf8(&mut out, "Code");
        utf8(&mut out, "LineNumberTable");
        utf8(&mut out, "LocalVariableTable");
        utf8(&mut out, "StackMapTable");
        u2(&mut out, 0x0021); // access
        u2(&mut out, 2); // this
        u2(&mut out, 4); // super
        u2(&mut out, 0); // interfaces
        u2(&mut out, 0); // fields
        u2(&mut out, 1); // methods
        u2(&mut out, 0x0001); // public
        u2(&mut out, 5);
        u2(&mut out, 6);
        u2(&mut out, 1); // attributes
        u2(&mut out, 7);

        let mut code_attributes = Vec::new();
        // LineNumberTable: (0, 1) and (5, 2).
        u2(&mut code_attributes, 8);
        let mut attribute = Vec::new();
        u2(&mut attribute, 2);
        for (start_pc, line) in [(0u16, 1u16), (5, 2)] {
            u2(&mut attribute, start_pc);
            u2(&mut attribute, line);
        }
        u4(&mut code_attributes, attribute.len() as u32);
        code_attributes.extend_from_slice(&attribute);
        // LocalVariableTable: slot 1 over [0, 7).
        u2(&mut code_attributes, 9);
        let mut attribute = Vec::new();
        u2(&mut attribute, 1);
        u2(&mut attribute, 0); // start_pc
        u2(&mut attribute, 7); // length
        u2(&mut attribute, 5); // name
        u2(&mut attribute, 6); // descriptor
        u2(&mut attribute, 1); // slot
        u4(&mut code_attributes, attribute.len() as u32);
        code_attributes.extend_from_slice(&attribute);
        // StackMapTable: one full frame at offset 5.
        u2(&mut code_attributes, 10);
        let mut attribute = Vec::new();
        u2(&mut attribute, 1);
        attribute.push(255);
        u2(&mut attribute, 5); // offset_delta
        u2(&mut attribute, 3); // locals
        attribute.push(7);
        u2(&mut attribute, 2); // demo/T
        attribute.push(1); // Integer
        attribute.push(1); // Integer
        u2(&mut attribute, 0); // stack
        u4(&mut code_attributes, attribute.len() as u32);
        code_attributes.extend_from_slice(&attribute);

        let mut body = Vec::new();
        u2(&mut body, 1); // max_stack
        u2(&mut body, 3); // max_locals
        u4(&mut body, 7);
        body.extend_from_slice(&[0x1b, 0x9a, 0x00, 0x04, 0xac, 0x1c, 0xac]);
        u2(&mut body, 0); // exception table
        u2(&mut body, 3); // Code attributes
        body.extend_from_slice(&code_attributes);
        u4(&mut out, body.len() as u32);
        out.extend_from_slice(&body);
        u2(&mut out, 0); // class attributes
        out
    }

    fn target() -> MethodRef {
        MethodRef {
            owner: "demo/T".into(),
            name: "m".into(),
            descriptor: "(II)I".into(),
        }
    }

    #[test]
    fn write_round_trips_a_synthetic_class() {
        let bytes = fixture(0x0001, "(II)I", 3, &[0x1a, 0x1b, 0x60, 0xac], &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        assert_eq!(class.write().unwrap(), bytes);
    }

    #[test]
    fn write_preserves_long_double_and_unknown_attributes() {
        let mut bytes = Vec::new();
        u4(&mut bytes, 0xCAFE_BABE);
        u2(&mut bytes, 0);
        u2(&mut bytes, 52);
        u2(&mut bytes, 10); // constant pool count (two wide slots included)
        utf8(&mut bytes, "demo/LD"); // 1
        bytes.extend_from_slice(&[7, 0, 1]); // 2
        utf8(&mut bytes, "java/lang/Object"); // 3
        bytes.extend_from_slice(&[7, 0, 3]); // 4
        bytes.extend_from_slice(&[5, 0, 0, 0, 0, 0, 0, 0, 7]); // 5: Long, slot 6
        bytes.extend_from_slice(&[6, 0x40, 0x09, 0x21, 0xfb, 0x54, 0x44, 0x2d, 0x18]); // 7: Double, slot 8
        utf8(&mut bytes, "Signature"); // 9
        u2(&mut bytes, 0x0021);
        u2(&mut bytes, 2);
        u2(&mut bytes, 4);
        u2(&mut bytes, 0); // interfaces
        u2(&mut bytes, 0); // fields
        u2(&mut bytes, 0); // methods
        u2(&mut bytes, 1); // class attributes
        u2(&mut bytes, 9);
        u4(&mut bytes, 4);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let class = ClassFile::parse(&bytes).unwrap();
        assert_eq!(class.write().unwrap(), bytes);
    }

    #[test]
    fn writer_rejects_an_unknown_constant_tag() {
        let mut bytes = fixture(0x0001, "(II)I", 3, &[0xac], &[]);
        // Turn the `Code` Utf8 (last entry) into tag 2, which the class writer
        // does not know.  `write_class` must not emit the bytes back.
        let code_name = bytes.windows(4).position(|w| w == b"Code").unwrap();
        bytes[code_name - 3] = 2;
        assert!(write_class(&bytes).is_err());
    }

    #[test]
    fn head_weaves_an_instance_method() {
        let bytes = fixture(0x0001, "(II)I", 3, &[0x1a, 0x1b, 0x60, 0xac], &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let woven = weave_head(&class, &target(), &callback("(Ldemo/T;II)V")).unwrap();
        let reparsed = ClassFile::parse(&woven).unwrap();
        assert_eq!(reparsed.write().unwrap(), woven);
        let method = reparsed.method("m", "(II)I").unwrap();
        let code = method.code.as_ref().unwrap();
        assert_eq!(&code[..5], &[0x2a, 0x15, 0x01, 0x15, 0x02]);
        assert_eq!(code[5], 0xb8);
        assert_eq!(&code[8..], &[0x1a, 0x1b, 0x60, 0xac]);
        assert_eq!(method.max_stack, 2 + 3 + 1);
    }

    #[test]
    fn head_weaves_an_instance_target_without_the_receiver() {
        let bytes = fixture(0x0001, "(II)I", 3, &[0x1a, 0x1b, 0x60, 0xac], &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let woven = weave_head(&class, &target(), &callback("(II)V")).unwrap();
        let reparsed = ClassFile::parse(&woven).unwrap();
        let method = reparsed.method("m", "(II)I").unwrap();
        let code = method.code.as_ref().unwrap();
        assert_eq!(&code[..4], &[0x15, 0x01, 0x15, 0x02]);
        assert_eq!(&code[7..], &[0x1a, 0x1b, 0x60, 0xac]);
        assert_eq!(method.max_stack, 2 + 2 + 1);
    }

    #[test]
    fn head_rejects_a_receiver_for_a_static_target() {
        let bytes = fixture(0x0009, "(II)I", 2, &[0x1a, 0x1b, 0x60, 0xac], &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let error = weave_head(&class, &target(), &callback("(Ldemo/T;II)V")).unwrap_err();
        assert!(error.contains("static target expects"), "{error}");
    }

    #[test]
    fn head_rejects_a_wrong_callback_descriptor() {
        let bytes = fixture(0x0001, "(II)I", 3, &[0x1a, 0x1b, 0x60, 0xac], &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let error = weave_head(&class, &target(), &callback("(Ldemo/T;I)V")).unwrap_err();
        assert!(error.contains("does not match"), "{error}");
    }

    const TEST_CALLBACK_INFO_DESC: &str =
        "Lorg/spongepowered/asm/mixin/injection/callback/CallbackInfo;";

    #[test]
    fn head_weaves_a_static_callback_info_argument() {
        let bytes = fixture(0x0001, "(II)I", 3, &[0x1a, 0x1b, 0x60, 0xac], &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let descriptor = format!("(II{TEST_CALLBACK_INFO_DESC})V");
        let plan = MixinInjectionPlan::inject_at_head(target(), callback(&descriptor));
        let (woven, added) = weave_class_with_extras(&class, std::slice::from_ref(&plan)).unwrap();
        assert!(
            added.contains_key(CALLBACK_INFO_CLASS),
            "{:?}",
            added.keys()
        );
        let (code, max_stack, _) = woven_code(&woven, "m", "(II)I");
        // iload_1; iload_2; new; dup; invokespecial; invokestatic.
        assert_eq!(&code[..4], &[0x15, 0x01, 0x15, 0x02]);
        assert_eq!(code[4], 0xbb);
        assert_eq!(code[7], 0x59);
        assert_eq!(code[8], 0xb7);
        assert_eq!(code[11], 0xb8);
        assert_eq!(&code[14..], &[0x1a, 0x1b, 0x60, 0xac]);
        assert_eq!(max_stack, 2 + 3 + 1);
        // The bytes-only entry point emits the same insert; the caller just
        // has to add `CallbackInfo` to the jar.
        assert_eq!(
            weave_head(&class, &target(), &callback(&descriptor)).unwrap(),
            woven
        );
    }

    #[test]
    fn return_weaves_a_static_callback_info_argument() {
        let bytes = fixture(0x0009, "(I)V", 1, &[0xb1], &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let descriptor = format!("(I{TEST_CALLBACK_INFO_DESC})V");
        let target = MethodRef {
            descriptor: "(I)V".into(),
            ..target()
        };
        let woven =
            weave_return(&class, &target, &callback(&descriptor), ReturnMode::Return).unwrap();
        let (code, _, max_locals) = woven_code(&woven, "m", "(I)V");
        // iload_0; new; dup; invokespecial; invokestatic; return.
        assert_eq!(&code[..2], &[0x15, 0x00]);
        assert_eq!(code[2], 0xbb);
        assert_eq!(code[5], 0x59);
        assert_eq!(code[6], 0xb7);
        assert_eq!(code[9], 0xb8);
        assert_eq!(code[12], 0xb1);
        assert_eq!(max_locals, 1, "the marker lives only on the stack");
    }

    #[test]
    fn an_instance_callback_is_reached_through_a_bridge() {
        let target_bytes = fixture(0x0001, "(II)I", 3, &[0x1a, 0x1b, 0x60, 0xac], &[]);
        let callback_descriptor = format!("(II{TEST_CALLBACK_INFO_DESC})V");
        let callback_bytes = named_fixture(
            "demo/Cb",
            "cb",
            0x0001,
            &callback_descriptor,
            4,
            &[0xb1],
            &[],
        );
        let mut classes = BTreeMap::new();
        classes.insert(
            "demo/T".to_owned(),
            ClassFile::parse(&target_bytes).unwrap(),
        );
        classes.insert(
            "demo/Cb".to_owned(),
            ClassFile::parse(&callback_bytes).unwrap(),
        );
        let plan = MixinInjectionPlan::inject_at_head(
            target(),
            MethodRef {
                owner: "demo/Cb".into(),
                name: "cb".into(),
                descriptor: callback_descriptor.clone(),
            },
        );
        let woven = weave_classes_with_extras(&classes, &[plan]).unwrap();
        assert!(woven.added.contains_key(CALLBACK_INFO_CLASS));
        let bridge_name = woven
            .added
            .keys()
            .find(|name| name.contains("HorizonBridge"))
            .expect("a bridge class")
            .clone();
        let bridge = ClassFile::parse(&woven.added[&bridge_name]).unwrap();
        assert_eq!(bridge.name, bridge_name);
        let field = bridge
            .fields
            .iter()
            .find(|field| field.name == "callback0")
            .expect("callback0 field");
        assert!(field.is_static());
        assert_eq!(field.descriptor, "Ldemo/Cb;");
        let callback = bridge.method("callback0", &callback_descriptor).unwrap();
        assert!(callback.is_static());
        let code = callback.code.as_ref().unwrap();
        assert_eq!(code[0], 0xb2, "getstatic the callback instance");
        assert_eq!(code[code.len() - 1], 0xb1);
        let install = bridge.method("install0", "(Ldemo/Cb;)V").unwrap();
        assert!(install.is_static());

        // The woven target calls the static bridge, not the instance method.
        let (code, _, _) = woven_code(&woven.classes["demo/T"], "m", "(II)I");
        let invoke = code
            .iter()
            .position(|byte| *byte == 0xb8)
            .expect("an invokestatic");
        let index = u16::from_be_bytes([code[invoke + 1], code[invoke + 2]]);
        let target_class = ClassFile::parse(&woven.classes["demo/T"]).unwrap();
        match &target_class.pool[index as usize] {
            Cp::MethodRef { class, nt } => {
                assert_eq!(
                    pool_class_at(&target_class.pool, *class),
                    Some(bridge_name.as_str())
                );
                let (name, descriptor) = pool_name_type_at(&target_class.pool, *nt).unwrap();
                assert_eq!(name, "callback0");
                assert_eq!(descriptor, callback_descriptor);
            }
            other => panic!("expected a MethodRef, got {other:?}"),
        }
    }

    #[test]
    fn a_private_instance_callback_is_a_reason() {
        let target_bytes = fixture(0x0009, "(I)V", 1, &[0xb1], &[]);
        let callback_bytes = named_fixture("demo/Cb", "cb", 0x0002, "(I)V", 2, &[0xb1], &[]);
        let mut classes = BTreeMap::new();
        classes.insert(
            "demo/T".to_owned(),
            ClassFile::parse(&target_bytes).unwrap(),
        );
        classes.insert(
            "demo/Cb".to_owned(),
            ClassFile::parse(&callback_bytes).unwrap(),
        );
        let plan = MixinInjectionPlan::inject_at_head(
            MethodRef {
                descriptor: "(I)V".into(),
                ..target()
            },
            MethodRef {
                owner: "demo/Cb".into(),
                name: "cb".into(),
                descriptor: "(I)V".into(),
            },
        );
        let error = weave_classes_with_extras(&classes, &[plan]).unwrap_err();
        assert!(error.contains("private"), "{error}");
    }

    /// Parse `Code` out of a woven class so the assertions read the written
    /// bytes, not the in-memory patch.
    fn woven_code(woven: &[u8], name: &str, descriptor: &str) -> (Vec<u8>, u16, u16) {
        let class = ClassFile::parse(woven).unwrap();
        assert_eq!(class.write().unwrap(), woven, "woven class round-trips");
        let method = class.method(name, descriptor).unwrap();
        (
            method.code.clone().unwrap(),
            method.max_stack,
            method.max_locals,
        )
    }

    fn jdk_tool(name: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("JAVA_HOME")
            .map(|home| std::path::PathBuf::from(home).join("bin").join(name))
            .unwrap_or_else(|| name.into());
        let output = std::process::Command::new(&path)
            .arg("-version")
            .output()
            .ok()?;
        output.status.success().then_some(path)
    }

    fn write_class_file(dir: &std::path::Path, name: &str, bytes: &[u8]) {
        let path = dir.join(format!("{name}.class"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// Compile the fixtures and load the woven classes on a real classloader.
    ///
    /// Structural byte checks cannot catch verifier errors in a shape that
    /// constructs an object and a bridge; this runs both the instance callback
    /// (through its bridge) and the static plain-`CallbackInfo` callback on a
    /// JVM.  A machine without a JDK skips cleanly.
    #[test]
    fn woven_callbacks_load_on_a_real_classloader() {
        let (Some(javac), Some(java)) = (jdk_tool("javac"), jdk_tool("java")) else {
            eprintln!("skipping: no JDK on PATH or JAVA_HOME");
            return;
        };
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-weaver-bridge-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        std::fs::create_dir_all(src.join("demo")).unwrap();
        std::fs::create_dir_all(&classes).unwrap();
        write_class_file(
            &classes,
            CALLBACK_INFO_CLASS,
            &callback_info_class().unwrap(),
        );
        std::fs::write(
            src.join("demo/Target.java"),
            r#"package demo;
public class Target {
    public static int calls = 0;
    public static void run(int x) { calls += 1; }
    public static void runStatic(int x) { calls += 1; }
}
"#,
        )
        .unwrap();
        std::fs::write(
            src.join("demo/MixinCb.java"),
            r#"package demo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
public final class MixinCb {
    public void onRun(int x, CallbackInfo ci) { Target.calls += x; }
    public static void onRunStatic(int x, CallbackInfo ci) { Target.calls += 10 + x; }
}
"#,
        )
        .unwrap();
        let output = std::process::Command::new(&javac)
            .args(["--release", "17", "-nowarn", "-cp"])
            .arg(&classes)
            .arg("-d")
            .arg(&classes)
            .arg(src.join("demo/Target.java"))
            .arg(src.join("demo/MixinCb.java"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "javac: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let mut set = BTreeMap::new();
        for name in ["demo/Target", "demo/MixinCb"] {
            let bytes = std::fs::read(classes.join(format!("{name}.class"))).unwrap();
            set.insert(name.to_owned(), ClassFile::parse(&bytes).unwrap());
        }
        let callback_descriptor = format!("(I{TEST_CALLBACK_INFO_DESC})V");
        let plans = [
            MixinInjectionPlan::inject_at_head(
                MethodRef {
                    owner: "demo/Target".into(),
                    name: "run".into(),
                    descriptor: "(I)V".into(),
                },
                MethodRef {
                    owner: "demo/MixinCb".into(),
                    name: "onRun".into(),
                    descriptor: callback_descriptor.clone(),
                },
            ),
            MixinInjectionPlan::inject_at_head(
                MethodRef {
                    owner: "demo/Target".into(),
                    name: "runStatic".into(),
                    descriptor: "(I)V".into(),
                },
                MethodRef {
                    owner: "demo/MixinCb".into(),
                    name: "onRunStatic".into(),
                    descriptor: callback_descriptor,
                },
            ),
        ];
        let woven = weave_classes_with_extras(&set, &plans).unwrap();
        let bridge_name = woven
            .added
            .keys()
            .find(|name| name.contains("HorizonBridge"))
            .expect("a bridge class")
            .clone();
        let runner = root.join("runner");
        std::fs::create_dir_all(&runner).unwrap();
        for (name, bytes) in &woven.classes {
            write_class_file(&runner, name, bytes);
        }
        for (name, bytes) in &woven.added {
            write_class_file(&runner, name, bytes);
        }

        let driver_src = root.join("driver-src/driven/Driver.java");
        std::fs::create_dir_all(driver_src.parent().unwrap()).unwrap();
        std::fs::write(
            &driver_src,
            r#"package driven;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Paths;
public final class Driver {
    public static void main(String[] args) throws Exception {
        URL[] urls = { Paths.get(args[0]).toUri().toURL() };
        try (URLClassLoader loader = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
            Class<?> mixin = Class.forName("demo.MixinCb", true, loader);
            Object instance = mixin.getDeclaredConstructor().newInstance();
            Class<?> bridge = Class.forName(args[1], true, loader);
            bridge.getMethod("install0", mixin).invoke(null, instance);
            Class<?> target = Class.forName("demo.Target", true, loader);
            target.getMethod("run", int.class).invoke(null, Integer.valueOf(17));
            target.getMethod("runStatic", int.class).invoke(null, Integer.valueOf(5));
            System.out.println("calls=" + target.getField("calls").getInt(null));
        }
    }
}
"#,
        )
        .unwrap();
        let driver = root.join("driver");
        std::fs::create_dir_all(&driver).unwrap();
        let output = std::process::Command::new(&javac)
            .args(["--release", "17", "-nowarn", "-d"])
            .arg(&driver)
            .arg(&driver_src)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "javac driver: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = std::process::Command::new(&java)
            .arg("-cp")
            .arg(&driver)
            .arg("driven.Driver")
            .arg(&runner)
            .arg(bridge_name.replace('/', "."))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "java: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        // run: 1 + 17; runStatic: 1 + 10 + 5.
        assert!(stdout.contains("calls=34"), "{stdout}");
    }

    #[test]
    fn head_relocates_a_branch_target() {
        // `ifne +3; return` — the branch jumps over the return to offset 3.
        let bytes = fixture(0x0001, "(I)I", 2, &[0x99, 0x00, 0x03, 0xb1], &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let branchy = MethodRef {
            descriptor: "(I)I".into(),
            ..target()
        };
        let woven = weave_head(&class, &branchy, &callback("(Ldemo/T;I)V")).unwrap();
        let (code, max_stack, max_locals) = woven_code(&woven, "m", "(I)I");
        // aload_0 (1) + iload_1 (2) + invokestatic (3) = 6 bytes.
        assert_eq!(&code[..6], &[0x2a, 0x15, 0x01, 0xb8, 0x00, 0x0d]);
        assert_eq!(code[6], 0x99);
        // The return moved to 9, and the branch now sits at 6.  JVM offsets are
        // relative to the opcode, so the operand is 9 - 6 = 3.
        assert_eq!(i16::from_be_bytes([code[7], code[8]]), 3);
        assert_eq!(code[9], 0xb1);
        assert_eq!(max_stack, 2 + 2 + 1);
        assert_eq!(max_locals, 2);
    }

    #[test]
    fn head_relocates_an_exception_table() {
        // `ifne +4; return; ireturn` with a handler covering [3, 5) that starts
        // at offset 4, so every column has to move by the insert.
        let mut handler = Vec::new();
        u2(&mut handler, 3); // start_pc
        u2(&mut handler, 5); // end_pc
        u2(&mut handler, 4); // handler_pc
        u2(&mut handler, 0); // catch_type: catch-all
        let bytes = fixture(
            0x0001,
            "(II)I",
            3,
            &[0x99, 0x00, 0x04, 0xb1, 0xac],
            &handler,
        );
        let class = ClassFile::parse(&bytes).unwrap();
        let woven = weave_head(&class, &target(), &callback("(Ldemo/T;II)V")).unwrap();
        let (code, _, _) = woven_code(&woven, "m", "(II)I");
        // aload_0 + iload_1 + iload_2 + invokestatic = 8 bytes.
        assert_eq!(code.len(), 5 + 8);
        // The table sits between the code and the two (empty) attribute counts.
        let table = &woven[woven.len() - 12..woven.len() - 4];
        assert_eq!(
            table,
            &[
                0, 11, // start_pc: 3 + 8
                0, 13, // end_pc: 5 + 8
                0, 12, // handler_pc: 4 + 8
                0, 0, // catch-all
            ]
        );
    }

    #[test]
    fn head_relocates_debug_tables_and_frames() {
        let bytes = frames_fixture();
        let class = ClassFile::parse(&bytes).unwrap();
        let woven = weave_head(&class, &target(), &callback("(Ldemo/T;II)V")).unwrap();
        let (_, _, _) = woven_code(&woven, "m", "(II)I");
        let layout = Layout::parse(&woven).unwrap();
        let method = &layout.methods[0];
        let code = method.attributes[0].code.as_ref().unwrap();
        let mut frame_lines = Vec::new();
        for attribute in &code.attributes {
            match attribute {
                CodeAttribute::LineNumbers { lines, .. } => {
                    for line in lines {
                        frame_lines.push(("line", line.start_pc));
                    }
                }
                CodeAttribute::LocalVariables { entries, .. } => {
                    for entry in entries {
                        frame_lines.push(("local", entry.start_pc));
                        frame_lines.push(("length", entry.length));
                    }
                }
                CodeAttribute::StackMap { frames, .. } => {
                    for frame in frames {
                        frame_lines.push(("frame", frame.offset as u16));
                    }
                }
                CodeAttribute::Raw { .. } => {}
            }
        }
        // The fixture inserts 8 bytes at offset 0; every recorded offset moves
        // with it and the local-variable range keeps its own length.
        assert_eq!(
            frame_lines,
            vec![
                ("line", 8),
                ("line", 13),
                ("local", 8),
                ("length", 7),
                ("frame", 13),
            ]
        );
    }

    #[test]
    fn head_relocates_a_tableswitch() {
        // 0: tableswitch (three padding bytes), default -> 24, low 0, high 1,
        // case 0 -> 25, case 1 -> 26; then three `ireturn`.
        let code = [
            0xaa, 0, 0, 0, // opcode and alignment padding
            0, 0, 0, 24, // default
            0, 0, 0, 0, // low
            0, 0, 0, 1, // high
            0, 0, 0, 25, // case 0
            0, 0, 0, 26, // case 1
            0xac, 0xac, 0xac,
        ];
        assert_eq!(code.len(), 27);
        let bytes = fixture(0x0001, "(II)I", 3, &code, &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        // `iload_1; iload_2; invokestatic` is 7 bytes, which is not a multiple
        // of four: the switch's padding has to be re-derived, so it shrinks from
        // 24 bytes to 21 and everything after it moves a second time.
        let woven = weave_head(&class, &target(), &callback("(II)V")).unwrap();
        let (woven_code, _, _) = woven_code(&woven, "m", "(II)I");
        assert_eq!(woven_code.len(), 27 + 7 - 3);
        assert_eq!(woven_code[7], 0xaa);
        assert_eq!(switch_header(&woven_code, 7), (21, 0, 1, vec![22, 23]));
        assert_eq!(&woven_code[28..], &[0xac, 0xac, 0xac]);
    }

    #[test]
    fn head_relocates_a_lookupswitch() {
        // 0: lookupswitch (three padding bytes), default -> 28, two pairs
        // (1 -> 29, 100 -> 30); then three `ireturn`.
        let code = [
            0xab, 0, 0, 0, // opcode and alignment padding
            0, 0, 0, 28, // default
            0, 0, 0, 2, // npairs
            0, 0, 0, 1, 0, 0, 0, 29, // key 1 -> 29
            0, 0, 0, 100, 0, 0, 0, 30, // key 100 -> 30
            0xac, 0xac, 0xac,
        ];
        assert_eq!(code.len(), 31);
        let bytes = fixture(0x0001, "(II)I", 3, &code, &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let woven = weave_head(&class, &target(), &callback("(II)V")).unwrap();
        let (woven_code, _, _) = woven_code(&woven, "m", "(II)I");
        // Padding drops from three bytes to none, so the switch shrinks 3 bytes
        // and the three returns land at 32, 33 and 34.
        assert_eq!(woven_code.len(), 31 + 7 - 3);
        assert_eq!(woven_code[7], 0xab);
        assert_eq!(woven_code[8..12], 25i32.to_be_bytes());
        assert_eq!(woven_code[12..16], 2i32.to_be_bytes());
        assert_eq!(woven_code[16..20], 1i32.to_be_bytes());
        assert_eq!(woven_code[20..24], 26i32.to_be_bytes());
        assert_eq!(woven_code[24..28], 100i32.to_be_bytes());
        assert_eq!(woven_code[28..32], 27i32.to_be_bytes());
        assert_eq!(&woven_code[32..], &[0xac, 0xac, 0xac]);
    }

    #[test]
    fn switch_padding_grows_when_an_insert_moves_a_switch_off_the_grid() {
        // The same jump table behind one `nop`, so the switch starts at offset 1
        // and needs two padding bytes.  A 7-byte insert puts it at 8, where it
        // needs three: the instruction grows by one on top of the insert.
        let code = [
            0x00, // nop
            0xaa, 0, 0, // opcode and two padding bytes
            0, 0, 0, 20, // default -> 21
            0, 0, 0, 0, // low
            0, 0, 0, 0, // high
            0, 0, 0, 19, // case 0 -> 20
            0xac, 0xac, // ireturn, ireturn
        ];
        assert_eq!(code.len(), 22);
        let bytes = fixture(0x0001, "(II)I", 3, &code, &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let woven = weave_head(&class, &target(), &callback("(II)V")).unwrap();
        let (woven_code, _, _) = woven_code(&woven, "m", "(II)I");
        assert_eq!(woven_code.len(), code.len() + 7 + 1);
        assert_eq!(&woven_code[..4], &[0x15, 0x01, 0x15, 0x02]);
        assert_eq!(woven_code[4], 0xb8); // invokestatic
        assert_eq!(woven_code[7], 0x00, "the nop moved to 7");
        assert_eq!(woven_code[8], 0xaa);
        // default -> 29, the case -> 28, both relative to the switch at 8.
        assert_eq!(switch_header(&woven_code, 8), (21, 0, 0, vec![20]));
        assert_eq!(&woven_code[28..], &[0xac, 0xac]);
    }

    #[test]
    fn head_weaves_a_method_with_wide_locals() {
        // `wide iload 256; wide iinc 257 by 3; ireturn`.  `javac` cannot emit
        // this (a source method's slots stop below 256), so it is assembled by
        // hand: the weaver has to walk the 4- and 6-byte wide forms instead of
        // treating `wide` as a one-byte opcode.
        let code = [
            0xc4, 0x15, 0x01, 0x00, // wide iload 256
            0xc4, 0x84, 0x01, 0x01, 0x00, 0x03, // wide iinc 257, 3
            0xac, // ireturn
        ];
        let bytes = fixture(0x0001, "()I", 260, &code, &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let target = MethodRef {
            owner: "demo/T".into(),
            name: "m".into(),
            descriptor: "()I".into(),
        };
        let woven = weave_head(&class, &target, &callback("(Ldemo/T;)V")).unwrap();
        let (woven_code, _, max_locals) = woven_code(&woven, "m", "()I");
        assert_eq!(woven_code.len(), code.len() + 4); // aload_0 + invokestatic
        assert_eq!(&woven_code[4..], &code);
        assert_eq!(max_locals, 260, "the wide slots raise nothing");
    }

    #[test]
    fn head_grows_a_goto_that_would_leave_the_16_bit_range() {
        // `goto +32767; return; 32763 nops; return`: the goto sits exactly at
        // the edge of its 16-bit operand.  A RETURN insert in front of the first
        // return pushes the target past it, so the goto has to become `goto_w`.
        let mut code = vec![0xa7, 0x7f, 0xff, 0xb1];
        code.resize(4 + 32_763, 0x00);
        code.push(0xb1);
        assert_eq!(code.len(), 32_768);
        let bytes = fixture(0x0009, "()V", 0, &code, &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let target = MethodRef {
            owner: "demo/T".into(),
            name: "m".into(),
            descriptor: "()V".into(),
        };
        let woven = weave_return(&class, &target, &callback("()V"), ReturnMode::Return).unwrap();
        let (woven_code, _, _) = woven_code(&woven, "m", "()V");
        // Two 3-byte inserts, plus the two bytes the widening adds.
        assert_eq!(woven_code.len(), code.len() + 6 + 2);
        assert_eq!(woven_code[0], 0xc8, "the goto grew to goto_w");
        // A RETURN target enters the callback: 32767 + 3 (the first insert) +
        // 2 (the wider goto), the inserted call itself starting there.
        assert_eq!(woven_code[1..5], 32_772i32.to_be_bytes());
        assert_eq!(woven_code[5], 0xb8, "the first callback sits before it");
        assert_eq!(woven_code[8], 0xb1);
        assert_eq!(*woven_code.last().unwrap(), 0xb1);
    }

    #[test]
    fn head_expands_a_conditional_branch_past_the_16_bit_range() {
        // `iconst_0; ifeq +32766; return; 32762 nops; return`: the branch sits
        // at 1 and names 32767.  Two RETURN inserts (at 4 and at 32767) push the
        // target to 32770, past the 16-bit range, so the `ifeq` is split into
        // `ifne +8; goto_w target` and the fallthrough - offset 4, now a branch
        // target - gets a synthesised frame.
        let mut code = vec![0x03, 0x99, 0x7f, 0xfe, 0xb1];
        code.resize(32_767, 0x00);
        code.push(0xb1);
        assert_eq!(code.len(), 32_768);
        let bytes = fixture(0x0009, "()V", 0, &code, &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let target = MethodRef {
            owner: "demo/T".into(),
            name: "m".into(),
            descriptor: "()V".into(),
        };
        let woven = weave_return(&class, &target, &callback("()V"), ReturnMode::Return).unwrap();
        let (woven_code, _, _) = woven_code(&woven, "m", "()V");
        assert_eq!(woven_code[0], 0x03, "iconst_0 stays first");
        assert_eq!(woven_code[1], 0x9a, "ifeq became ifne");
        assert_eq!(
            i16::from_be_bytes([woven_code[2], woven_code[3]]),
            8,
            "the inverse branch skips the wide jump"
        );
        assert_eq!(woven_code[4], 0xc8, "the far target uses goto_w");
        assert_eq!(
            i32::from_be_bytes([woven_code[5], woven_code[6], woven_code[7], woven_code[8]]),
            32_771,
            "goto_w names the relocated target"
        );
        // The fallthrough return moved to 9 and starts with its callback.
        assert_eq!(woven_code[9], 0xb8);

        // The method had no StackMapTable, so the split adds one with a single
        // full frame at the fallthrough.
        let layout = Layout::parse(&woven).unwrap();
        let method = &layout.methods[0];
        let code = method.attributes[0].code.as_ref().unwrap();
        let frames = code
            .attributes
            .iter()
            .find_map(|attribute| match attribute {
                CodeAttribute::StackMap { frames, .. } => Some(frames),
                _ => None,
            })
            .expect("a synthesised StackMapTable");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].offset, 9);
        assert_eq!(frames[0].frame_type, 255);
        assert!(frames[0].locals.is_empty());
        assert!(frames[0].stack.is_empty());
    }

    /// Cancellable `RETURN`/`TAIL` on a non-void target.
    ///
    /// Mixin's cancellable handler returns `getReturnValue()` whether or not the
    /// callback cancelled, so `cancel()` alone keeps the target's own result and
    /// a `setReturnValue` still wins - exactly what the branch-free insert does.
    /// Only a `void` target still needs the interpreter's runtime-owned
    /// `CallbackInfo`.
    #[test]
    fn a_cancellable_plan_weaves_a_non_void_target() {
        let bytes = fixture(0x0009, "(I)I", 1, &[0x1a, 0xac], &[]);
        let class = ClassFile::parse(&bytes).unwrap();
        let target = MethodRef {
            owner: "demo/T".into(),
            name: "m".into(),
            descriptor: "(I)I".into(),
        };
        let mut plan = MixinInjectionPlan::inject_at_return(
            target.clone(),
            callback(&format!("(I{CALLBACK_INFO_RETURNABLE_DESC})V")),
        );
        plan.cancellable = true;
        let woven = weave_class(&class, &[plan]).unwrap();
        let (code, _, _) = woven_code(&woven, "m", "(I)I");
        assert_eq!(
            code.iter().filter(|opcode| **opcode == 0xbb).count(),
            1,
            "one CallbackInfoReturnable for the single return"
        );
        assert_eq!(code.last(), Some(&0xac), "the plain return stays last");
        assert_eq!(
            code.iter().filter(|opcode| **opcode == 0xb6).count(),
            2,
            "getReturnValue plus the unboxing call"
        );
    }

    /// The synthetic class is a normal class file the crate's own parser reads,
    /// so a jar that carries it loads on the native path too.
    #[test]
    fn the_synthetic_returnable_parses_and_round_trips() {
        let bytes = callback_info_returnable_class().unwrap();
        let class = ClassFile::parse(&bytes).unwrap();
        assert_eq!(class.name, CALLBACK_INFO_RETURNABLE_CLASS);
        assert_eq!(class.write().unwrap(), bytes);
        assert_eq!(class.methods.len(), 6);
        assert_eq!(class.fields.len(), 2);
        assert!(
            class
                .method("getReturnValue", "()Ljava/lang/Object;")
                .is_some()
        );
        assert!(
            class
                .method("setReturnValue", "(Ljava/lang/Object;)V")
                .is_some()
        );
    }

    /// `(default, low, high, cases)` of the `tableswitch` at `at`, with the
    /// relative offsets kept relative to the opcode.
    fn switch_header(code: &[u8], at: usize) -> (i32, i32, i32, Vec<i32>) {
        assert_eq!(code[at], 0xaa);
        let mut field = at + 1 + super::switch_padding(at);
        let mut read = || {
            let value = i32::from_be_bytes([
                code[field],
                code[field + 1],
                code[field + 2],
                code[field + 3],
            ]);
            field += 4;
            value
        };
        let default = read();
        let low = read();
        let high = read();
        let cases = (low..=high).map(|_| read()).collect();
        (default, low, high, cases)
    }

    /// A minimal class with one `CONSTANT_Dynamic`: pool index 19 is the condy,
    /// its name/type (index 10) is `condyName`/`I`, bootstrap row 0 is a
    /// `ConstantBootstraps`-shaped handle (index 18) with one `String` argument
    /// (index 20, whose `Utf8` is index 11).
    fn condy_fixture() -> Vec<u8> {
        let mut out = Vec::new();
        u4(&mut out, 0xCAFE_BABE);
        u2(&mut out, 0);
        u2(&mut out, 52);
        u2(&mut out, 22);
        utf8(&mut out, "demo/C"); // 1
        out.extend_from_slice(&[7, 0, 1]); // 2
        utf8(&mut out, "java/lang/Object"); // 3
        out.extend_from_slice(&[7, 0, 3]); // 4
        utf8(&mut out, "m"); // 5
        utf8(&mut out, "()V"); // 6
        utf8(&mut out, "Code"); // 7
        utf8(&mut out, "condyName"); // 8
        utf8(&mut out, "I"); // 9
        out.extend_from_slice(&[12, 0, 8, 0, 9]); // 10 NameAndType
        utf8(&mut out, "extra"); // 11
        utf8(&mut out, "java/lang/invoke/ConstantBootstraps"); // 12
        utf8(&mut out, "bootstrap"); // 13
        utf8(&mut out, "()Ljava/lang/Object;"); // 14
        out.extend_from_slice(&[12, 0, 13, 0, 14]); // 15 NameAndType
        out.extend_from_slice(&[7, 0, 12]); // 16 Class
        out.extend_from_slice(&[10, 0, 16, 0, 15]); // 17 Methodref
        out.extend_from_slice(&[15, 6, 0, 17]); // 18 MethodHandle
        out.extend_from_slice(&[17, 0, 0, 0, 10]); // 19 Dynamic
        out.extend_from_slice(&[8, 0, 11]); // 20 String
        utf8(&mut out, "BootstrapMethods"); // 21

        u2(&mut out, 0x0021); // access
        u2(&mut out, 2); // this
        u2(&mut out, 4); // super
        u2(&mut out, 0); // interfaces
        u2(&mut out, 0); // fields
        u2(&mut out, 1); // methods
        u2(&mut out, 0x0001);
        u2(&mut out, 5);
        u2(&mut out, 6);
        u2(&mut out, 1); // attributes
        u2(&mut out, 7); // Code
        let mut body = Vec::new();
        u2(&mut body, 0); // max_stack
        u2(&mut body, 0); // max_locals
        u4(&mut body, 1); // code_length
        body.push(0xb1); // return
        u2(&mut body, 0); // exception table
        u2(&mut body, 0); // Code attributes
        u4(&mut out, body.len() as u32);
        out.extend_from_slice(&body);

        u2(&mut out, 1); // class attributes
        u2(&mut out, 21);
        let mut bootstrap = Vec::new();
        u2(&mut bootstrap, 1); // rows
        u2(&mut bootstrap, 18); // method handle
        u2(&mut bootstrap, 1); // static arguments
        u2(&mut bootstrap, 20);
        u4(&mut out, bootstrap.len() as u32);
        out.extend_from_slice(&bootstrap);
        out
    }

    #[test]
    fn condy_pools_are_accepted_and_round_trip() {
        let bytes = condy_fixture();
        let class = ClassFile::parse(&bytes).unwrap();
        assert_eq!(class.write().unwrap(), bytes, "identity round-trip");
        let layout = Layout::parse(&bytes).unwrap();
        layout
            .validate_constant_pool()
            .expect("a well-formed condy is inside the writer's subset");
    }

    /// A relocated pool rewrites `CONSTANT_Dynamic`'s name/type and the
    /// `BootstrapMethods` static argument, so a future compaction cannot leave
    /// the condy pointing at the wrong constant.
    #[test]
    fn pool_relocation_rewrites_dynamic_and_bootstrap_references() {
        let bytes = condy_fixture();
        let layout = Layout::parse(&bytes).unwrap();
        let identity = PoolMap::identity(layout.pool_count);

        let mut pool = Vec::new();
        layout.write_pool(&mut pool, &identity).unwrap();
        assert_eq!(
            &bytes[10..10 + pool.len()],
            pool.as_slice(),
            "the identity map reproduces the pool bytes"
        );
        let mut attributes = Vec::new();
        layout
            .write_class_attributes(&mut attributes, &identity)
            .unwrap();
        assert_eq!(
            &bytes[bytes.len() - attributes.len()..],
            attributes.as_slice(),
            "the identity map reproduces the BootstrapMethods bytes"
        );

        let mut map = PoolMap::identity(layout.pool_count);
        map.0.swap(8, 11);
        let mut relocated = Vec::new();
        layout.write_pool(&mut relocated, &map).unwrap();
        let mut r = Bytes::new(&relocated);
        let (utf8, entries) = parse_pool(&mut r, layout.pool_count).unwrap();
        assert_eq!(utf8[8], Some("extra"));
        assert_eq!(utf8[11], Some("condyName"));
        let entry = |index: u16| {
            entries
                .iter()
                .find(|entry| entry.index == index)
                .expect("relocated entry")
        };
        assert_eq!(entry(10).tag, 12);
        assert_eq!(payload_u2(entry(10).payload, 0).unwrap(), 11);
        assert_eq!(payload_u2(entry(10).payload, 2).unwrap(), 9);
        assert_eq!(entry(19).tag, 17);
        assert_eq!(payload_u2(entry(19).payload, 0).unwrap(), 0);
        assert_eq!(payload_u2(entry(19).payload, 2).unwrap(), 10);
        assert_eq!(entry(20).tag, 8);
        assert_eq!(payload_u2(entry(20).payload, 0).unwrap(), 8);

        let mut relocated_attributes = Vec::new();
        layout
            .write_class_attributes(&mut relocated_attributes, &map)
            .unwrap();
        assert_eq!(
            relocated_attributes, attributes,
            "the bootstrap row keeps its handle and argument indices"
        );
    }
    fn compile_java_sources(
        javac: &std::path::Path,
        src: &std::path::Path,
        classes: &std::path::Path,
        sources: &[(&str, &str)],
    ) {
        let mut paths = Vec::new();
        for (name, source) in sources {
            let path = src.join(format!("{name}.java"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, source).unwrap();
            paths.push(path);
        }
        let output = std::process::Command::new(javac)
            .args(["--release", "17", "-nowarn", "-d"])
            .arg(classes)
            .args(&paths)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "javac: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn read_classes(classes: &std::path::Path, names: &[&str]) -> BTreeMap<String, ClassFile> {
        names
            .iter()
            .map(|name| {
                let bytes = std::fs::read(classes.join(format!("{name}.class"))).unwrap();
                ((*name).to_owned(), ClassFile::parse(&bytes).unwrap())
            })
            .collect()
    }

    fn anchor_plan(
        target: MethodRef,
        callback: MethodRef,
        at: &str,
        site: Option<&str>,
        opcode: Option<u8>,
        redirect: bool,
    ) -> MixinAnchorPlan {
        MixinAnchorPlan {
            target,
            callback,
            at: at.to_owned(),
            site: site.map(str::to_owned),
            opcode,
            cancellable: false,
            ordinal: None,
            slice_from: None,
            slice_to: None,
            redirect,
        }
    }

    fn method_ref(owner: &str, name: &str, descriptor: &str) -> MethodRef {
        MethodRef {
            owner: owner.to_owned(),
            name: name.to_owned(),
            descriptor: descriptor.to_owned(),
        }
    }

    /// Compile a target with every extended anchor family, weave it, and run
    /// the result on a real classloader.
    ///
    /// The test is the verifier check for the emission shapes: FIELD inserts
    /// (read and write), NEW, JUMP, @Overwrite, @ModifyArg, @ModifyVariable at
    /// STORE and LOAD, and HEAD with a shift and with a slice.
    #[test]
    fn extended_anchors_load_on_a_real_classloader() {
        let (Some(javac), Some(java)) = (jdk_tool("javac"), jdk_tool("java")) else {
            eprintln!("skipping: no JDK on PATH or JAVA_HOME");
            return;
        };
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-weaver-extended-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&classes).unwrap();
        compile_java_sources(
            &javac,
            &src,
            &classes,
            &[
                (
                    "demo/Anchors",
                    r#"package demo;
public class Anchors {
    public static int value = 5;
    public static int hits = 0;
    public static int read() { return value; }
    public static void write(int x) { value = x; }
    public static int make() { return new Object() == null ? 0 : 1; }
    public static int branch(int x) { if (x > 0) { return 10; } return 20; }
    public static int overwritten(int x) { return x + 1; }
    public static int call(int x) { return twice(x); }
    static int twice(int x) { return x * 2; }
    public static int store(int x) { int y = x + 1; return y * 3; }
    public static int load(int x) { int y = x + 1; return y * 3; }
    public static int shifted(int x) { int y = x + 1; return y * 2; }
    public static int sliced(int x) { int y = x + 1; return y * 2; }
    public static void writeValued(int x) { value = x; }
    public static int writeBranch(int x) { value = x; if (x > 0) { return 1; } return 2; }
    public static int join(int a, int b) { return pair(a, b); }
    static int pair(int a, int b) { return a * 10 + b; }
    public static int fullMod(int a, int b) { return pair(a, b); }
    public static int localWithArg(int x) { int y = x + 1; if (x > 0) { return y * 3; } return 0; }
    public static int redirectedRead(int x) { if (x > 0) { x = 1; } else { x = 2; } return value; }
    public static void redirectedWrite(int x) { value = x; }
    public static int redirectCall(int x) { return twice(x); }
}
"#,
                ),
                (
                    "demo/Cb",
                    r#"package demo;
public class Cb {
    public static void onRead() { Anchors.hits += 100; }
    public static void onWrite(int x) { Anchors.hits += 1; }
    public static void onNew() { Anchors.hits += 1000; }
    public static void onBranch(int x) { Anchors.hits += 10000 + x; }
    public static void onShift(int x) { Anchors.hits += 100000 + x; }
    public static void onSliced(int x) { Anchors.hits += 1000000 + x; }
    public static int overwrite(int x) { return x * 10; }
    public static int plusOne(int x) { return x + 1; }
    public static int timesTwo(int x) { return x * 2; }
    public static int minusOne(int x) { return x - 1; }
    public static void onWriteValued(int x, int value) { Anchors.hits += value; }
    public static int addB(int a, int b) { return b + 100; }
    public static int valuePlusArg(int value, int x) { return value * 10 + x; }
    public static int readValue(int x) { return 99; }
    public static void dropWrite(int targetArg, int written) { Anchors.value = 42; }
    public static int fakeTwice(int x) { return x * 5; }
}
"#,
                ),
            ],
        );
        let set = read_classes(&classes, &["demo/Anchors", "demo/Cb"]);
        let mut read_plan = anchor_plan(
            method_ref("demo/Anchors", "read", "()I"),
            method_ref("demo/Cb", "onRead", "()V"),
            "FIELD",
            Some("demo/Anchors#value:I"),
            Some(0xb2),
            false,
        );
        read_plan.slice_from = Some("HEAD".to_owned());
        read_plan.slice_to = Some("RETURN".to_owned());
        let mut new_plan = anchor_plan(
            method_ref("demo/Anchors", "make", "()I"),
            method_ref("demo/Cb", "onNew", "()V"),
            "NEW",
            Some("java/lang/Object"),
            None,
            false,
        );
        new_plan.slice_from = Some("HEAD".to_owned());
        new_plan.slice_to = Some("RETURN".to_owned());
        let mut jump_plan = anchor_plan(
            method_ref("demo/Anchors", "branch", "(I)I"),
            method_ref("demo/Cb", "onBranch", "(I)V"),
            "JUMP",
            None,
            Some(0x9e),
            false,
        );
        jump_plan.slice_from = Some("HEAD".to_owned());
        jump_plan.slice_to = Some("RETURN".to_owned());
        let anchors = vec![
            read_plan,
            anchor_plan(
                method_ref("demo/Anchors", "write", "(I)V"),
                method_ref("demo/Cb", "onWrite", "(I)V"),
                "FIELD",
                Some("demo/Anchors#value:I"),
                Some(0xb3),
                false,
            ),
            anchor_plan(
                method_ref("demo/Anchors", "writeValued", "(I)V"),
                method_ref("demo/Cb", "onWriteValued", "(II)V"),
                "FIELD",
                Some("demo/Anchors#value:I"),
                Some(0xb3),
                false,
            ),
            anchor_plan(
                method_ref("demo/Anchors", "writeBranch", "(I)I"),
                method_ref("demo/Cb", "onWriteValued", "(II)V"),
                "FIELD",
                Some("demo/Anchors#value:I"),
                Some(0xb3),
                false,
            ),
            new_plan,
            jump_plan,
            anchor_plan(
                method_ref("demo/Anchors", "redirectedRead", "(I)I"),
                method_ref("demo/Cb", "readValue", "(I)I"),
                "FIELD",
                Some("demo/Anchors#value:I"),
                Some(0xb2),
                true,
            ),
            anchor_plan(
                method_ref("demo/Anchors", "redirectedWrite", "(I)V"),
                method_ref("demo/Cb", "dropWrite", "(II)V"),
                "FIELD",
                Some("demo/Anchors#value:I"),
                Some(0xb3),
                true,
            ),
            anchor_plan(
                method_ref("demo/Anchors", "redirectCall", "(I)I"),
                method_ref("demo/Cb", "fakeTwice", "(I)I"),
                "INVOKE",
                Some("demo/Anchors#twice(I)I"),
                None,
                true,
            ),
        ];
        let mut overwrite = MixinInjectionPlan::overwrite(
            method_ref("demo/Anchors", "overwritten", "(I)I"),
            method_ref("demo/Cb", "overwrite", "(I)I"),
        );
        let mut shifted = MixinInjectionPlan::inject_at_head(
            method_ref("demo/Anchors", "shifted", "(I)I"),
            method_ref("demo/Cb", "onShift", "(I)V"),
        );
        shifted.shift = Some(InvokeShift {
            direction: Shift::After,
            by: 0,
        });
        let mut sliced = MixinInjectionPlan::inject_at_head(
            method_ref("demo/Anchors", "sliced", "(I)I"),
            method_ref("demo/Cb", "onSliced", "(I)V"),
        );
        sliced.slice = Some(SliceBounds {
            from: SiteSelector::Head,
            to: SiteSelector::Return,
        });
        overwrite.slice = None;
        let injections = vec![overwrite, shifted, sliced];
        let modify_args = vec![
            MixinModifyArgPlan {
                target: method_ref("demo/Anchors", "call", "(I)I"),
                callback: method_ref("demo/Cb", "plusOne", "(I)I"),
                invoke_target: method_ref("demo/Anchors", "twice", "(I)I"),
                index: 0,
                ordinal: None,
            },
            MixinModifyArgPlan {
                target: method_ref("demo/Anchors", "join", "(II)I"),
                callback: method_ref("demo/Cb", "plusOne", "(I)I"),
                invoke_target: method_ref("demo/Anchors", "pair", "(II)I"),
                index: 0,
                ordinal: None,
            },
            MixinModifyArgPlan {
                target: method_ref("demo/Anchors", "fullMod", "(II)I"),
                callback: method_ref("demo/Cb", "addB", "(II)I"),
                invoke_target: method_ref("demo/Anchors", "pair", "(II)I"),
                index: 1,
                ordinal: None,
            },
        ];
        let modify_variables = vec![
            MixinModifyVariablePlan {
                target: method_ref("demo/Anchors", "store", "(I)I"),
                callback: method_ref("demo/Cb", "timesTwo", "(I)I"),
                at: "STORE".to_owned(),
                index: Some(1),
                ordinal: None,
            },
            MixinModifyVariablePlan {
                target: method_ref("demo/Anchors", "load", "(I)I"),
                callback: method_ref("demo/Cb", "minusOne", "(I)I"),
                at: "LOAD".to_owned(),
                index: Some(1),
                ordinal: None,
            },
            MixinModifyVariablePlan {
                target: method_ref("demo/Anchors", "localWithArg", "(I)I"),
                callback: method_ref("demo/Cb", "valuePlusArg", "(II)I"),
                at: "STORE".to_owned(),
                index: Some(1),
                ordinal: None,
            },
        ];
        let woven =
            weave_extended_classes(&set, &injections, &anchors, &modify_args, &modify_variables)
                .unwrap();
        let runner = root.join("runner");
        std::fs::create_dir_all(&runner).unwrap();
        for (name, bytes) in &woven.classes {
            write_class_file(&runner, name, bytes);
        }
        for (name, bytes) in &woven.added {
            write_class_file(&runner, name, bytes);
        }
        let driver_src = root.join("driver-src/driven/Driver.java");
        std::fs::create_dir_all(driver_src.parent().unwrap()).unwrap();
        std::fs::write(
            &driver_src,
            r#"package driven;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Paths;
public final class Driver {
    public static void main(String[] args) throws Exception {
        URL[] urls = { Paths.get(args[0]).toUri().toURL() };
        try (URLClassLoader loader = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
            Class<?> target = Class.forName("demo.Anchors", true, loader);
            System.out.println(
                target.getMethod("read").invoke(null)
                + "," + target.getMethod("make").invoke(null)
                + "," + target.getMethod("branch", int.class).invoke(null, Integer.valueOf(3))
                + "," + target.getMethod("overwritten", int.class).invoke(null, Integer.valueOf(4))
                + "," + target.getMethod("call", int.class).invoke(null, Integer.valueOf(3))
                + "," + target.getMethod("store", int.class).invoke(null, Integer.valueOf(4))
                + "," + target.getMethod("load", int.class).invoke(null, Integer.valueOf(4))
                + "," + target.getMethod("shifted", int.class).invoke(null, Integer.valueOf(2))
                + "," + target.getMethod("sliced", int.class).invoke(null, Integer.valueOf(2))
                + "," + target.getMethod("join", int.class, int.class).invoke(null, Integer.valueOf(3), Integer.valueOf(4))
                + "," + target.getMethod("fullMod", int.class, int.class).invoke(null, Integer.valueOf(3), Integer.valueOf(4))
                + "," + target.getMethod("localWithArg", int.class).invoke(null, Integer.valueOf(2))
                + "," + target.getMethod("redirectedRead", int.class).invoke(null, Integer.valueOf(1))
                + "," + target.getMethod("redirectCall", int.class).invoke(null, Integer.valueOf(3))
                + "," + target.getField("hits").getInt(null));
            target.getMethod("redirectedWrite", int.class).invoke(null, Integer.valueOf(8));
            System.out.println(target.getField("value").getInt(null));
            target.getMethod("write", int.class).invoke(null, Integer.valueOf(9));
            target.getMethod("writeValued", int.class).invoke(null, Integer.valueOf(6));
            target.getMethod("writeBranch", int.class).invoke(null, Integer.valueOf(7));
            System.out.println(target.getField("value").getInt(null) + "," + target.getField("hits").getInt(null));
        }
    }
}
"#,
        )
        .unwrap();
        let driver = root.join("driver");
        std::fs::create_dir_all(&driver).unwrap();
        let output = std::process::Command::new(&javac)
            .args(["--release", "17", "-nowarn", "-d"])
            .arg(&driver)
            .arg(&driver_src)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "javac driver: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = std::process::Command::new(&java)
            .arg("-cp")
            .arg(&driver)
            .arg("driven.Driver")
            .arg(&runner)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "java: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        // read 5, make 1, branch 10, overwritten 40, call 8, store 30, load 12,
        // shifted 6, sliced 6, join 44, fullMod 134, localWithArg 96,
        // redirectedRead 99, redirectCall 15.  hits: 100 + 1000 + (10000+3) + (100000+2) +
        // (1000000+2) before the first print, then +1 (write), +6 (valued
        // write) and +7 (a valued write with a stack-map frame after it).
        assert!(
            stdout.contains("5,1,10,40,8,30,12,6,6,44,134,96,99,15,1111107"),
            "{stdout}"
        );
        assert!(stdout.contains("42"), "{stdout}");
        assert!(stdout.contains("7,1111121"), "{stdout}");
    }

    /// `AnchorRequest.shift` moves a FIELD insert off the access instruction,
    /// which is the seam discovery does not fill yet: `MixinAnchorPlan` has no
    /// shift field, so the caller passes [`ShiftBy`] directly.
    #[test]
    fn anchor_shift_moves_a_field_insert_on_a_real_classloader() {
        let (Some(javac), Some(java)) = (jdk_tool("javac"), jdk_tool("java")) else {
            eprintln!("skipping: no JDK on PATH or JAVA_HOME");
            return;
        };
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-weaver-anchor-shift-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&classes).unwrap();
        compile_java_sources(
            &javac,
            &src,
            &classes,
            &[
                (
                    "demo/Shifty",
                    r#"package demo;
public class Shifty {
    public static int value = 4;
    public static int hits = 0;
    public static int readShift() { return value; }
}
"#,
                ),
                (
                    "demo/ShiftCb",
                    r#"package demo;
public class ShiftCb {
    public static void onRead() { Shifty.hits += 100; }
}
"#,
                ),
            ],
        );
        let bytes = std::fs::read(classes.join("demo/Shifty.class")).unwrap();
        let class = ClassFile::parse(&bytes).unwrap();
        let request = AnchorRequest {
            site: AnchorSite::Field(FieldSite {
                access: FieldAccess::StaticGet,
                owner: Some("demo/Shifty".to_owned()),
                name: "value".to_owned(),
                descriptor: Some("I".to_owned()),
            }),
            ordinal: None,
            slice: None,
            shift: Some(ShiftBy { by: 1 }),
            redirect: false,
            cancellable: false,
        };
        let woven = weave_anchor(
            &class,
            &method_ref("demo/Shifty", "readShift", "()I"),
            &method_ref("demo/ShiftCb", "onRead", "()V"),
            &request,
        )
        .unwrap();
        let runner = root.join("runner");
        std::fs::create_dir_all(&runner).unwrap();
        write_class_file(&runner, "demo/Shifty", &woven);
        write_class_file(
            &runner,
            "demo/ShiftCb",
            &std::fs::read(classes.join("demo/ShiftCb.class")).unwrap(),
        );
        let driver_src = root.join("driver-src/driven/Driver.java");
        std::fs::create_dir_all(driver_src.parent().unwrap()).unwrap();
        std::fs::write(
            &driver_src,
            r#"package driven;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Paths;
public final class Driver {
    public static void main(String[] args) throws Exception {
        URL[] urls = { Paths.get(args[0]).toUri().toURL() };
        try (URLClassLoader loader = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
            Class<?> target = Class.forName("demo.Shifty", true, loader);
            System.out.println(
                target.getMethod("readShift").invoke(null)
                + "," + target.getField("hits").getInt(null));
        }
    }
}
"#,
        )
        .unwrap();
        let driver = root.join("driver");
        std::fs::create_dir_all(&driver).unwrap();
        let output = std::process::Command::new(&javac)
            .args(["--release", "17", "-nowarn", "-d"])
            .arg(&driver)
            .arg(&driver_src)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "javac driver: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = std::process::Command::new(&java)
            .arg("-cp")
            .arg(&driver)
            .arg("driven.Driver")
            .arg(&runner)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "java: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("4,100"),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// `@ModifyArg` on a constructor call rewrites an argument while the
    /// caller's `new`/`dup` pair keeps the uninitialized reference on the
    /// operand stack below it.
    #[test]
    fn modify_arg_rewrites_a_constructor_argument() {
        let (Some(javac), Some(java)) = (jdk_tool("javac"), jdk_tool("java")) else {
            eprintln!("skipping: no JDK on PATH or JAVA_HOME");
            return;
        };
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-weaver-ctor-arg-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&classes).unwrap();
        compile_java_sources(
            &javac,
            &src,
            &classes,
            &[
                (
                    "demo/CtorTarget",
                    r#"package demo;
public class CtorTarget {
    public static int make(int x) { return new Box(x).value; }
}
class Box { public int value; Box(int v) { value = v; } }
"#,
                ),
                (
                    "demo/CtorCb",
                    r#"package demo;
public class CtorCb {
    public static int plusOne(int x) { return x + 1; }
}
"#,
                ),
            ],
        );
        let set = read_classes(&classes, &["demo/CtorTarget", "demo/Box", "demo/CtorCb"]);
        let modify_args = vec![MixinModifyArgPlan {
            target: method_ref("demo/CtorTarget", "make", "(I)I"),
            callback: method_ref("demo/CtorCb", "plusOne", "(I)I"),
            invoke_target: method_ref("demo/Box", "<init>", "(I)V"),
            index: 0,
            ordinal: None,
        }];
        let woven = weave_extended_classes(&set, &[], &[], &modify_args, &[]).unwrap();
        let runner = root.join("runner");
        std::fs::create_dir_all(&runner).unwrap();
        for (name, bytes) in &woven.classes {
            write_class_file(&runner, name, bytes);
        }
        for (name, bytes) in &woven.added {
            write_class_file(&runner, name, bytes);
        }
        let driver_src = root.join("driver-src/driven/Driver.java");
        std::fs::create_dir_all(driver_src.parent().unwrap()).unwrap();
        std::fs::write(
            &driver_src,
            r#"package driven;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Paths;
public final class Driver {
    public static void main(String[] args) throws Exception {
        URL[] urls = { Paths.get(args[0]).toUri().toURL() };
        try (URLClassLoader loader = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
            Class<?> target = Class.forName("demo.CtorTarget", true, loader);
            System.out.println(target.getMethod("make", int.class).invoke(null, Integer.valueOf(3)));
        }
    }
}
"#,
        )
        .unwrap();
        let driver = root.join("driver");
        std::fs::create_dir_all(&driver).unwrap();
        let output = std::process::Command::new(&javac)
            .args(["--release", "17", "-nowarn", "-d"])
            .arg(&driver)
            .arg(&driver_src)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "javac driver: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = std::process::Command::new(&java)
            .arg("-cp")
            .arg(&driver)
            .arg("driven.Driver")
            .arg(&runner)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "java: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains('4'),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// A non-void instance callback behind an `@ModifyArg` goes through the
    /// bridge, which has to `xreturn` the value the woven call expects.
    #[test]
    fn an_instance_modify_arg_bridge_loads_on_a_real_classloader() {
        let (Some(javac), Some(java)) = (jdk_tool("javac"), jdk_tool("java")) else {
            eprintln!("skipping: no JDK on PATH or JAVA_HOME");
            return;
        };
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-weaver-modify-bridge-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&classes).unwrap();
        compile_java_sources(
            &javac,
            &src,
            &classes,
            &[
                (
                    "demo/ArgTarget",
                    r#"package demo;
public class ArgTarget {
    public static int call(int x) { return twice(x); }
    static int twice(int x) { return x * 2; }
}
"#,
                ),
                (
                    "demo/ArgMixin",
                    r#"package demo;
public class ArgMixin {
    public int plusOne(int x) { return x + 1; }
}
"#,
                ),
            ],
        );
        let set = read_classes(&classes, &["demo/ArgTarget", "demo/ArgMixin"]);
        let modify_args = vec![MixinModifyArgPlan {
            target: method_ref("demo/ArgTarget", "call", "(I)I"),
            callback: method_ref("demo/ArgMixin", "plusOne", "(I)I"),
            invoke_target: method_ref("demo/ArgTarget", "twice", "(I)I"),
            index: 0,
            ordinal: None,
        }];
        let woven = weave_extended_classes(&set, &[], &[], &modify_args, &[]).unwrap();
        let bridge = woven
            .added
            .keys()
            .find(|name| name.contains("HorizonBridge"))
            .expect("a non-void bridge class")
            .clone();
        let runner = root.join("runner");
        std::fs::create_dir_all(&runner).unwrap();
        for (name, bytes) in &woven.classes {
            write_class_file(&runner, name, bytes);
        }
        for (name, bytes) in &woven.added {
            write_class_file(&runner, name, bytes);
        }
        let driver_src = root.join("driver-src/driven/Driver.java");
        std::fs::create_dir_all(driver_src.parent().unwrap()).unwrap();
        std::fs::write(
            &driver_src,
            r#"package driven;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Paths;
public final class Driver {
    public static void main(String[] args) throws Exception {
        URL[] urls = { Paths.get(args[0]).toUri().toURL() };
        try (URLClassLoader loader = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
            Class<?> mixin = Class.forName("demo.ArgMixin", true, loader);
            Object instance = mixin.getDeclaredConstructor().newInstance();
            Class<?> bridge = Class.forName(args[1], true, loader);
            bridge.getMethod("install0", mixin).invoke(null, instance);
            Class<?> target = Class.forName("demo.ArgTarget", true, loader);
            System.out.println(target.getMethod("call", int.class).invoke(null, Integer.valueOf(3)));
        }
    }
}
"#,
        )
        .unwrap();
        let driver = root.join("driver");
        std::fs::create_dir_all(&driver).unwrap();
        let output = std::process::Command::new(&javac)
            .args(["--release", "17", "-nowarn", "-d"])
            .arg(&driver)
            .arg(&driver_src)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "javac driver: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = std::process::Command::new(&java)
            .arg("-cp")
            .arg(&driver)
            .arg("driven.Driver")
            .arg(&runner)
            .arg(bridge.replace('/', "."))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "java: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // twice(3 + 1) = 8.
        assert!(
            String::from_utf8_lossy(&output.stdout).contains('8'),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
