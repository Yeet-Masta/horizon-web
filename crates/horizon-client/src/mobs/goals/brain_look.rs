//! The brain's `RandomLookAround` and the `LookAtTargetSink` that follows it.
use super::*;

/// `RandomLookAround`, the brain behaviour `CamelAi` and `ArmadilloAi` use,
/// which is not the goal of that name: every `interval` ticks it looks at
/// most `max_yaw` degrees off the body's yaw, at a pitch in the range, and
/// `LookAtTargetSink` keeps the head turning there for 45 to 90 ticks.
///
/// It holds `LOOK`: vanilla's behaviour starts only while `LOOK_TARGET` is
/// absent, and a temptation or a player to look at sets that memory, so a
/// higher `LOOK` goal (`Tempt`, `LookAtPlayer`) keeps it from starting and
/// stops the held look. `CountDownCooldownTicks` runs in the core activity
/// whatever looks, so the cooldown is the mob age it ends at and keeps
/// running while the goal is stopped.
#[derive(Clone, Debug)]
pub struct BrainLookAround {
    /// `UniformInt`'s bounds, in ticks.
    interval: (i32, i32),
    max_yaw: f32,
    min_pitch: f32,
    pitch_range: f32,
    /// The mob age from which `GAZE_COOLDOWN_TICKS` is gone and the next
    /// look can start.
    ready_at: u32,
    /// `LookAtTargetSink`'s remaining duration.
    hold: i32,
    /// `LOOK_TARGET`: the point the head turns to.
    target: [f64; 3],
}
impl BrainLookAround {
    /// `new RandomLookAround(interval, maxYaw, minPitch, maxPitch)`.
    pub fn new(interval: (i32, i32), max_yaw: f32, min_pitch: f32, max_pitch: f32) -> Self {
        Self {
            interval,
            max_yaw,
            min_pitch,
            pitch_range: max_pitch - min_pitch,
            ready_at: 0,
            hold: 0,
            target: [0.0; 3],
        }
    }
}
impl Goal for BrainLookAround {
    goal_clone!();
    fn flags(&self) -> u8 {
        LOOK
    }
    fn can_use(&mut self, _: &mut Mob, _: &mut Cx) -> bool {
        true
    }
    fn can_continue(&mut self, _: &mut Mob, _: &mut Cx) -> bool {
        true
    }
    fn stop(&mut self, _: &mut Mob, _: &mut Cx) {
        // The tempting player or the looked-at player took `LOOK_TARGET`'s
        // place: the random point is no longer looked at.
        self.hold = 0;
    }
    fn every_tick(&self) -> bool {
        true
    }
    fn tick(&mut self, m: &mut Mob, cx: &mut Cx) {
        // LookAtTargetSink: it turns the head to the remembered point, and
        // erases it when its duration runs out.
        if self.hold > 0 {
            self.hold -= 1;
            if self.hold > 0 {
                m.look_at(self.target);
            }
        }
        // CountDownCooldownTicks: the memory is erased at 0, and the look can
        // start the tick after.
        if m.age < self.ready_at {
            return;
        }
        // Behavior.tryStart draws the behaviour's duration (60, 60) from the
        // level's random, before `RandomLookAround.start`.
        let _ = cx.level.random().next_int_bound(1);
        let pitch = (m.random.next_float() * self.pitch_range + self.min_pitch).clamp(-90.0, 90.0);
        let rotation = crate::mobs::living::wrap(
            m.yaw + 2.0 * m.random.next_float() * self.max_yaw - self.max_yaw,
        );
        let dir = Vec3::direction_from_rotation(pitch, rotation);
        self.target = [
            m.position.x + dir.x,
            m.eye_y() + dir.y,
            m.position.z + dir.z,
        ];
        m.look_at(self.target);
        let cooldown = self.interval.0
            + m.random
                .next_int_bound(self.interval.1 - self.interval.0 + 1)
                .unwrap_or(0);
        self.ready_at = m.age.saturating_add(cooldown as u32 + 1);
        // LookAtTargetSink(45, 90)'s own duration, drawn when it starts.
        self.hold = 45 + cx.level.random().next_int_bound(46).unwrap_or(0);
    }
}
