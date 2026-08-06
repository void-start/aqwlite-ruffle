use std::cell::Cell;

thread_local! {
    static DRAW_CALLS: Cell<u32> = Cell::new(0);
    static BLEND_TRIVIAL: Cell<u32> = Cell::new(0);
    static BLEND_COMPLEX: Cell<u32> = Cell::new(0);
    static BLEND_SHADER: Cell<u32> = Cell::new(0);
    static CACHE_REDRAWS: Cell<u32> = Cell::new(0);
    static MAX_DRAW_CALLS: Cell<u32> = Cell::new(0);
    static FRAME_COUNT: Cell<u32> = Cell::new(0);
}

const LOG_EVERY_N_FRAMES: u32 = 60;

pub fn record_draw_call() {
    DRAW_CALLS.with(|c| c.set(c.get() + 1));
}

pub fn record_blend_trivial() {
    BLEND_TRIVIAL.with(|c| c.set(c.get() + 1));
}

pub fn record_blend_complex() {
    BLEND_COMPLEX.with(|c| c.set(c.get() + 1));
}

pub fn record_blend_shader() {
    BLEND_SHADER.with(|c| c.set(c.get() + 1));
}

pub fn begin_frame(cache_redraws: u32) {
    CACHE_REDRAWS.with(|c| c.set(cache_redraws));
}

pub fn end_frame() {
    let draw_calls = DRAW_CALLS.with(|c| c.replace(0));
    let blend_trivial = BLEND_TRIVIAL.with(|c| c.replace(0));
    let blend_complex = BLEND_COMPLEX.with(|c| c.replace(0));
    let blend_shader = BLEND_SHADER.with(|c| c.replace(0));
    let cache_redraws = CACHE_REDRAWS.with(|c| c.get());

    MAX_DRAW_CALLS.with(|m| {
        if draw_calls > m.get() {
            m.set(draw_calls);
        }
    });

    let frame = FRAME_COUNT.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        n
    });

    if frame % LOG_EVERY_N_FRAMES == 0 {
        let max_draw_calls = MAX_DRAW_CALLS.with(|m| m.replace(0));
        tracing::info!(
            "[stats] draw_calls={draw_calls} max_draw_calls_in_window={max_draw_calls} blend_trivial={blend_trivial} blend_complex={blend_complex} blend_shader={blend_shader} cache_redraws={cache_redraws}"
        );
    }
}
