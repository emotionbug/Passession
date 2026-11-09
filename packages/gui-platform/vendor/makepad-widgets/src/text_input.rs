use {
    crate::{
        makepad_derive_widget::*,
        makepad_draw::{
            text::{
                geom::Point,
                selection::{
                    Cursor,
                    CursorPosition,
                    Selection
                },
                layouter::LaidoutText,
            },
            *
        },
        widget::*,
    },
    std::rc::Rc,
    unicode_segmentation::{GraphemeCursor, UnicodeSegmentation},
};
use crate::makepad_draw::event::DigitId;


live_design! {
    link widgets;

    use link::theme::*;
    use makepad_draw::shader::std::*;
    // Explicit import retains the lazily constructed handle shader in the
    // expanded dependency graph; wildcard imports only retain used identifiers.
    use crate::text_selection_handles::DrawTextSelectionHandle;

    pub TextInputBase = {{TextInput}} {}
    
    pub TextInput = <TextInputBase> {
        width: Fill, height: Fit,
        padding: <THEME_MSPACE_1> { left: (THEME_SPACE_2), right: (THEME_SPACE_2) }
        margin: <THEME_MSPACE_V_1> {}
        flow: RightWrap,
        is_password: false,
        is_read_only: false,
        is_numeric_only: false
        empty_text: "Your text here",
        
        draw_bg: {
            instance hover: 0.0
            instance focus: 0.0
            instance down: 0.0
            instance empty: 0.0
            instance disabled: 0.0

            uniform border_radius: (THEME_CORNER_RADIUS)
            uniform border_size: (THEME_BEVELING)

            uniform color_dither: 1.0

            color: (THEME_COLOR_INSET)
            uniform color_hover: (THEME_COLOR_INSET_HOVER)
            uniform color_focus: (THEME_COLOR_INSET_FOCUS)
            uniform color_down: (THEME_COLOR_INSET_DOWN)
            uniform color_empty: (THEME_COLOR_INSET_EMPTY)
            uniform color_disabled: (THEME_COLOR_INSET_DISABLED)

            uniform border_color_1: (THEME_COLOR_BEVEL_INSET_2)
            uniform border_color_1_hover: (THEME_COLOR_BEVEL_INSET_2_HOVER)
            uniform border_color_1_focus: (THEME_COLOR_BEVEL_INSET_2_FOCUS)
            uniform border_color_1_down: (THEME_COLOR_BEVEL_INSET_2_DOWN)
            uniform border_color_1_empty: (THEME_COLOR_BEVEL_INSET_2_EMPTY)
            uniform border_color_1_disabled: (THEME_COLOR_BEVEL_INSET_2_DISABLED)

            uniform border_color_2: (THEME_COLOR_BEVEL_INSET_1)
            uniform border_color_2_hover: (THEME_COLOR_BEVEL_INSET_1_HOVER)
            uniform border_color_2_focus: (THEME_COLOR_BEVEL_INSET_1_FOCUS)
            uniform border_color_2_down: (THEME_COLOR_BEVEL_INSET_1_DOWN)
            uniform border_color_2_empty: (THEME_COLOR_BEVEL_INSET_1_EMPTY)
            uniform border_color_2_disabled: (THEME_COLOR_BEVEL_INSET_1_DISABLED)

            fn pixel(self) -> vec4 {
                let sdf = Sdf2d::viewport(self.pos * self.rect_size);
                let dither = Math::random_2d(self.pos.xy) * 0.04 * self.color_dither;
                
                let border_sz_uv = vec2(
                    self.border_size / self.rect_size.x,
                    self.border_size / self.rect_size.y
                )

                let scale_factor_border = vec2(
                    self.rect_size.x / self.rect_size.x,
                    self.rect_size.y / self.rect_size.y
                );

                let gradient_border = vec2(
                    self.pos.x * scale_factor_border.x + dither,
                    self.pos.y * scale_factor_border.y + dither
                )

                let sz_inner_px = vec2(
                    self.rect_size.x - self.border_size * 2.,
                    self.rect_size.y - self.border_size * 2.
                );

                let scale_factor_fill = vec2(
                    self.rect_size.x / sz_inner_px.x,
                    self.rect_size.y / sz_inner_px.y
                );

                let gradient_fill = vec2(
                    self.pos.x * scale_factor_fill.x - border_sz_uv.x * 2. + dither,
                    self.pos.y * scale_factor_fill.y - border_sz_uv.y * 2. + dither
                )

                sdf.box(
                    self.border_size,
                    self.border_size,
                    self.rect_size.x - self.border_size * 2.,
                    self.rect_size.y - self.border_size * 2.,
                    self.border_radius
                )

                sdf.fill_keep(
                    mix(
                        mix(
                            mix(
                                mix(
                                    self.color,
                                    self.color_empty,
                                    self.empty
                                ),
                                self.color_focus,
                                self.focus
                            ),
                            mix(
                                self.color_hover,
                                self.color_down,
                                self.down
                            ),
                            self.hover
                        ),
                        self.color_disabled,
                        self.disabled
                    )
                );

                sdf.stroke(
                    mix(
                        mix(
                            mix(
                                mix(
                                    mix(self.border_color_1, self.border_color_2, gradient_border.y),
                                    mix(self.border_color_1_empty, self.border_color_2_empty, gradient_border.y),
                                    self.empty
                                ),
                                mix(self.border_color_1_focus, self.border_color_2_focus, gradient_border.y),
                                self.focus
                            ),
                            mix(
                                mix(self.border_color_1_hover, self.border_color_2_hover, gradient_border.y),
                                mix(self.border_color_1_down, self.border_color_2_down, gradient_border.y),
                                self.down
                            ),
                            self.hover
                        ),
                        mix(self.border_color_1_disabled, self.border_color_2_disabled, gradient_border.y),
                        self.disabled
                    ),
                    self.border_size
                );

                
                return sdf.result;
            }
        }

        draw_text: {
            instance hover: 0.0
            instance focus: 0.0
            instance down: 0.0
            instance empty: 0.0
            instance disabled: 0.0

            color: (THEME_COLOR_TEXT)
            uniform color_hover: (THEME_COLOR_TEXT_HOVER)
            uniform color_focus: (THEME_COLOR_TEXT_FOCUS)
            uniform color_down: (THEME_COLOR_TEXT_DOWN)
            uniform color_disabled: (THEME_COLOR_TEXT_DISABLED)
            uniform color_empty: (THEME_COLOR_TEXT_PLACEHOLDER)
            uniform color_empty_hover: (THEME_COLOR_TEXT_PLACEHOLDER_HOVER)
            uniform color_empty_focus: (THEME_COLOR_TEXT_FOCUS)

            text_style: <THEME_FONT_REGULAR> {
                line_spacing: (THEME_FONT_WDGT_LINE_SPACING),
                font_size: (THEME_FONT_SIZE_P)
            }

            fn get_color(self) -> vec4 {
                return
                    mix( 
                        mix(
                            mix(
                                mix(
                                    self.color,
                                    mix(
                                        self.color_hover,
                                        self.color_down,
                                        self.down
                                    ),
                                    self.hover
                                ),
                                self.color_empty,
                                self.empty
                            ),
                            self.color_focus,
                            self.focus
                        ),
                        self.color_disabled,
                        self.disabled
                    )
            }
        }

        draw_selection: {
            instance hover: 0.0
            instance focus: 0.0
            instance down: 0.0
            instance empty: 0.0
            instance disabled: 0.0

            uniform border_radius: (THEME_TEXTSELECTION_CORNER_RADIUS)

            uniform color: (THEME_COLOR_SELECTION)
            uniform color_hover: (THEME_COLOR_SELECTION_HOVER)
            uniform color_focus: (THEME_COLOR_SELECTION_FOCUS)
            uniform color_down: (THEME_COLOR_SELECTION_DOWN)
            uniform color_empty: (THEME_COLOR_SELECTION_EMPTY)
            uniform color_disabled: (THEME_COLOR_SELECTION_DISABLED)

            fn pixel(self) -> vec4 {
                let sdf = Sdf2d::viewport(self.pos * self.rect_size);
                sdf.box(
                    0.0,
                    0.0,
                    self.rect_size.x,
                    self.rect_size.y,
                    self.border_radius
                );
                sdf.fill(
                    mix(
                        mix(
                            mix(
                                mix(
                                    self.color,
                                    self.color_empty,
                                    self.empty
                                ),
                                self.color_focus,
                                self.focus
                            ),
                            mix(
                                self.color_hover,
                                self.color_down,
                                self.down
                            ),
                            self.hover
                        ),
                        self.color_disabled,
                        self.disabled
                    )
                );
                return sdf.result;
            }
        }

        draw_cursor: {
            instance focus: 0.0
            instance down: 0.0
            instance empty: 0.0
            instance disabled: 0.0
            instance blink: 0.0
            
            uniform border_radius: 0.5

            uniform color: (THEME_COLOR_TEXT_CURSOR)

            fn pixel(self) -> vec4 {
                let sdf = Sdf2d::viewport(self.pos * self.rect_size);
                sdf.box(
                    0.0,
                    0.0,
                    self.rect_size.x,
                    self.rect_size.y,
                    self.border_radius
                );
                sdf.fill(
                    mix(THEME_COLOR_U_HIDDEN, self.color, (1.0-self.blink) * self.focus)
                );
                return sdf.result;
            }
        }

        animator: {
            empty = {
                default: off,
                off = {
                    from: {all: Forward {duration: 0.}}
                    apply: {
                        draw_bg: {empty: 0.0}
                        draw_text: {empty: 0.0}
                        draw_selection: {empty: 0.0}
                        draw_cursor: {empty: 0.0}
                    }
                }
                on = {
                    from: {all: Forward {duration: 0.2}}
                    apply: {
                        draw_bg: {empty: 1.0}
                        draw_text: {empty: 1.0}
                        draw_selection: {empty: 1.0}
                        draw_cursor: {empty: 1.0}
                    }
                }
            }
            blink = {
                default: off
                off = {
                    from: {all: Forward {duration:0.05}}
                    apply: {
                        draw_cursor: {blink:0.0}
                    }
                }
                on = {
                    from: {all: Forward {duration: 0.05}}
                    apply: {
                        draw_cursor: {blink:1.0}
                    }
                }
            }
            hover = {
                default: off,
                off = {
                    from: {all: Forward {duration: 0.1}}
                    apply: {
                        draw_bg: {down: 0.0, hover: 0.0}
                        draw_text: {down: 0.0, hover: 0.0}
                    }
                }
                
                on = {
                    from: {
                        all: Forward {duration: 0.1}
                        down: Forward {duration: 0.01}
                    }
                    apply: {
                        draw_bg: {down: 0.0, hover: [{time: 0.0, value: 1.0}],}
                        draw_text: {down: 0.0, hover: [{time: 0.0, value: 1.0}],}
                    }
                }
                
                down = {
                    from: {all: Forward {duration: 0.2}}
                    apply: {
                        draw_bg: {down: [{time: 0.0, value: 1.0}], hover: 1.0,}
                        draw_text: {down: [{time: 0.0, value: 1.0}], hover: 1.0,}
                    }
                }
            }
            disabled = {
                default: off,
                off = {
                    from: {all: Forward {duration: 0.}}
                    apply: {
                        draw_bg: {disabled: 0.0}
                        draw_text: {disabled: 0.0}
                        draw_selection: {disabled: 0.0}
                        draw_cursor: {disabled: 0.0}
                    }
                }
                on = {
                    from: {all: Forward {duration: 0.2}}
                    apply: {
                        draw_bg: {disabled: 1.0}
                        draw_text: {disabled: 1.0}
                        draw_selection: {disabled: 1.0}
                        draw_cursor: {disabled: 1.0}
                    }
                }
            }
            hover = {
                default: off,
                off = {
                    from: {all: Forward {duration: 0.1}}
                    apply: {
                        draw_bg: {down: 0.0, hover: 0.0}
                        draw_text: {down: 0.0, hover: 0.0}
                    }
                }
                
                on = {
                    from: {
                        all: Forward {duration: 0.1}
                        down: Forward {duration: 0.01}
                    }
                    apply: {
                        draw_bg: {down: 0.0, hover: [{time: 0.0, value: 1.0}],}
                        draw_text: {down: 0.0, hover: [{time: 0.0, value: 1.0}],}
                    }
                }
                
                down = {
                    from: {all: Forward {duration: 0.2}}
                    apply: {
                        draw_bg: {down: [{time: 0.0, value: 1.0}], hover: 1.0,}
                        draw_text: {down: [{time: 0.0, value: 1.0}], hover: 1.0,}
                    }
                }
            }
            focus = {
                default: off
                off = {
                    from: {
                        all: Forward { duration: 0.25 }
                    }
                    apply: {
                        draw_bg: { focus: 0.0 }
                        draw_text: { focus: 0.0 },
                        draw_cursor: { focus: 0.0 },
                        draw_selection: { focus: 0.0 }
                    }
                }
                on = {
                    from: { all: Snap }
                    apply: {
                        draw_bg: { focus: 1.0 }
                        draw_text: { focus: 1.0 }
                        draw_cursor: { focus: 1.0 },
                        draw_selection: { focus: 1.0 }
                    }
                }
            }
        }
    }

    pub TextInputFlat = <TextInput> {
        draw_bg: {
            border_color_1: (THEME_COLOR_BEVEL)
            border_color_1_hover: (THEME_COLOR_BEVEL_HOVER)
            border_color_1_focus: (THEME_COLOR_BEVEL_FOCUS)
            border_color_1_down: (THEME_COLOR_BEVEL_DOWN)
            border_color_1_empty: (THEME_COLOR_BEVEL_EMPTY)
            border_color_1_disabled: (THEME_COLOR_BEVEL_DISABLED)

            border_color_2: (THEME_COLOR_BEVEL)
            border_color_2_hover: (THEME_COLOR_BEVEL_HOVER)
            border_color_2_focus: (THEME_COLOR_BEVEL_FOCUS)
            border_color_2_down: (THEME_COLOR_BEVEL_DOWN)
            border_color_2_empty: (THEME_COLOR_BEVEL_EMPTY)
            border_color_2_disabled: (THEME_COLOR_BEVEL_DISABLED)
        }
    }

    pub TextInputFlatter = <TextInputFlat> { draw_bg: { border_size: 0. } }

    pub TextInputGradientX = <TextInput> {
        draw_bg: {
            instance hover: 0.0
            instance focus: 0.0
            instance down: 0.0
            instance disabled: 0.0
            instance empty: 0.0

            uniform border_radius: (THEME_CORNER_RADIUS)
            uniform border_size: (THEME_BEVELING)

            uniform color_dither: 1.0

            uniform color_1: (THEME_COLOR_INSET_1)
            uniform color_1_hover: (THEME_COLOR_INSET_1_HOVER)
            uniform color_1_focus: (THEME_COLOR_INSET_1_FOCUS)
            uniform color_1_down: (THEME_COLOR_INSET_1_DOWN)
            uniform color_1_empty: (THEME_COLOR_INSET_1_EMPTY)
            uniform color_1_disabled: (THEME_COLOR_INSET_1_DISABLED)

            uniform color_2: (THEME_COLOR_INSET_2)
            uniform color_2_hover: (THEME_COLOR_INSET_2_HOVER)
            uniform color_2_focus: (THEME_COLOR_INSET_2_FOCUS)
            uniform color_2_down: (THEME_COLOR_INSET_2_DOWN)
            uniform color_2_empty: (THEME_COLOR_INSET_2_EMPTY)
            uniform color_2_disabled: (THEME_COLOR_INSET_2_DISABLED)

            uniform border_color_1: (THEME_COLOR_BEVEL_INSET_2)
            uniform border_color_1_hover: (THEME_COLOR_BEVEL_INSET_2_HOVER)
            uniform border_color_1_focus: (THEME_COLOR_BEVEL_INSET_2_FOCUS)
            uniform border_color_1_down: (THEME_COLOR_BEVEL_INSET_2_DOWN)
            uniform border_color_1_empty: (THEME_COLOR_BEVEL_INSET_2_EMPTY)
            uniform border_color_1_disabled: (THEME_COLOR_BEVEL_INSET_2_DISABLED)

            uniform border_color_2: (THEME_COLOR_BEVEL_INSET_1)
            uniform border_color_2_hover: (THEME_COLOR_BEVEL_INSET_1_HOVER)
            uniform border_color_2_focus: (THEME_COLOR_BEVEL_INSET_1_FOCUS)
            uniform border_color_2_down: (THEME_COLOR_BEVEL_INSET_1_DOWN)
            uniform border_color_2_empty: (THEME_COLOR_BEVEL_INSET_1_EMPTY)
            uniform border_color_2_disabled: (THEME_COLOR_BEVEL_INSET_1_DISABLED)

            fn pixel(self) -> vec4 {
                let sdf = Sdf2d::viewport(self.pos * self.rect_size);
                let dither = Math::random_2d(self.pos.xy) * 0.04 * self.color_dither;

                let border_sz_uv = vec2(
                    self.border_size / self.rect_size.x,
                    self.border_size / self.rect_size.y
                )

                let scale_factor_border = vec2(
                    self.rect_size.x / self.rect_size.x,
                    self.rect_size.y / self.rect_size.y
                );

                let gradient_border = vec2(
                    self.pos.x * scale_factor_border.x + dither,
                    self.pos.y * scale_factor_border.y + dither
                )

                let sz_inner_px = vec2(
                    self.rect_size.x - self.border_size * 2.,
                    self.rect_size.y - self.border_size * 2.
                );

                let scale_factor_fill = vec2(
                    self.rect_size.x / sz_inner_px.x,
                    self.rect_size.y / sz_inner_px.y
                );

                let gradient_fill = vec2(
                    self.pos.x * scale_factor_fill.x - border_sz_uv.x * 2. + dither,
                    self.pos.y * scale_factor_fill.y - border_sz_uv.y * 2. + dither
                )
                
                sdf.box(
                    self.border_size,
                    self.border_size,
                    self.rect_size.x - self.border_size * 2.,
                    self.rect_size.y - self.border_size * 2.,
                    self.border_radius
                )

                sdf.fill_keep(
                    mix(
                        mix(
                            mix(
                                mix(
                                    mix(self.color_1, self.color_2, gradient_fill.x),
                                    mix(self.color_1_empty, self.color_2_empty, gradient_fill.x),
                                    self.empty
                                ),
                                mix(self.color_1_focus, self.color_2_focus, gradient_fill.x),
                                self.focus
                            ),
                            mix(
                                mix(self.color_1_hover, self.color_2_hover, gradient_fill.x),
                                mix(self.color_1_down, self.color_2_down, gradient_fill.x),
                                self.down
                            ),
                            self.hover
                        ),
                        mix(self.color_1_disabled, self.color_2_disabled, gradient_fill.x),
                        self.disabled
                    )
                );

                sdf.stroke(
                    mix(
                        mix(
                            mix(
                                mix(
                                    mix(self.border_color_1, self.border_color_2, gradient_border.y),
                                    mix(self.border_color_1_empty, self.border_color_2_empty, gradient_border.y),
                                    self.empty
                                ),
                                mix(self.border_color_1_focus, self.border_color_2_focus, gradient_border.y),
                                self.focus
                            ),
                            mix(
                                mix(self.border_color_1_hover, self.border_color_2_hover, gradient_border.y),
                                mix(self.border_color_1_down, self.border_color_2_down, gradient_border.y),
                                self.down
                            ),
                            self.hover
                        ),
                        mix(self.border_color_1_disabled, self.border_color_2_disabled, gradient_border.y),
                        self.disabled
                    ),
                    self.border_size
                );
                
                return sdf.result
            }
        }

        draw_selection: {
            instance hover: 0.0
            instance focus: 0.0
            instance down: 0.0
            instance disabled: 0.0
            instance empty: 0.0

            uniform border_radius: (THEME_TEXTSELECTION_CORNER_RADIUS)

            uniform color_1: (THEME_COLOR_SELECTION)
            uniform color_1_hover: (THEME_COLOR_SELECTION_HOVER)
            uniform color_1_focus: (THEME_COLOR_SELECTION_FOCUS)
            uniform color_1_down: (THEME_COLOR_SELECTION_DOWN)
            uniform color_1_empty: (THEME_COLOR_SELECTION_EMPTY)
            uniform color_1_disabled: (THEME_COLOR_SELECTION_DISABLED)

            uniform color_2: (THEME_COLOR_SELECTION)
            uniform color_2_hover: (THEME_COLOR_SELECTION_HOVER)
            uniform color_2_focus: (THEME_COLOR_SELECTION_FOCUS)
            uniform color_2_down: (THEME_COLOR_SELECTION_DOWN)
            uniform color_2_empty: (THEME_COLOR_SELECTION_EMPTY)
            uniform color_2_disabled: (THEME_COLOR_SELECTION_DISABLED)

            fn pixel(self) -> vec4 {
                let sdf = Sdf2d::viewport(self.pos * self.rect_size);

                sdf.box(
                    0.0,
                    0.0,
                    self.rect_size.x,
                    self.rect_size.y,
                    self.border_radius
                )

                sdf.fill(
                    mix(
                        mix(
                            mix(
                                mix(
                                    mix(self.color_1, self.color_2, self.pos.x),
                                    mix(self.color_1_empty, self.color_2_empty, self.pos.x),
                                    self.empty
                                ),
                                mix(self.color_1_focus, self.color_2_focus, self.pos.x),
                                self.focus
                            ),
                            mix(
                                mix(self.color_1_hover, self.color_2_hover, self.pos.x),
                                mix(self.color_1_down, self.color_2_down, self.pos.x),
                                self.down
                            ),
                            self.hover
                        ),
                        mix(self.color_1_disabled, self.color_2_disabled, self.pos.x),
                        self.disabled
                    )
                );

                return sdf.result
            }
        }
    }
        

    pub TextInputGradientY = <TextInputGradientX> {
        draw_bg: {
            fn pixel(self) -> vec4 {
                let sdf = Sdf2d::viewport(self.pos * self.rect_size);
                let dither = Math::random_2d(self.pos.xy) * 0.04 * self.color_dither;

                let border_sz_uv = vec2(
                    self.border_size / self.rect_size.x,
                    self.border_size / self.rect_size.y
                )

                let scale_factor_border = vec2(
                    self.rect_size.x / self.rect_size.x,
                    self.rect_size.y / self.rect_size.y
                );

                let gradient_border = vec2(
                    self.pos.x * scale_factor_border.x + dither,
                    self.pos.y * scale_factor_border.y + dither
                )

                let sz_inner_px = vec2(
                    self.rect_size.x - self.border_size * 2.,
                    self.rect_size.y - self.border_size * 2.
                );

                let scale_factor_fill = vec2(
                    self.rect_size.x / sz_inner_px.x,
                    self.rect_size.y / sz_inner_px.y
                );

                let gradient_fill = vec2(
                    self.pos.x * scale_factor_fill.x - border_sz_uv.x * 2. + dither,
                    self.pos.y * scale_factor_fill.y - border_sz_uv.y * 2. + dither
                )
                
                sdf.box(
                    self.border_size,
                    self.border_size,
                    self.rect_size.x - self.border_size * 2.,
                    self.rect_size.y - self.border_size * 2.,
                    self.border_radius
                )

                sdf.fill_keep(
                    mix(
                        mix(
                            mix(
                                mix(
                                    mix(self.color_1, self.color_2, gradient_fill.y),
                                    mix(self.color_1_empty, self.color_2_empty, gradient_fill.y),
                                    self.empty
                                ),
                                mix(self.color_1_focus, self.color_2_focus, gradient_fill.y),
                                self.focus
                            ),
                            mix(
                                mix(self.color_1_hover, self.color_2_hover, gradient_fill.y),
                                mix(self.color_1_down, self.color_2_down, gradient_fill.y),
                                self.down
                            ),
                            self.hover
                        ),
                        mix(self.color_1_disabled, self.color_2_disabled, gradient_fill.y),
                        self.disabled
                    )
                );

                sdf.stroke(
                    mix(
                        mix(
                            mix(
                                mix(
                                    mix(self.border_color_1, self.border_color_2, gradient_border.y),
                                    mix(self.border_color_1_empty, self.border_color_2_empty, gradient_border.y),
                                    self.empty
                                ),
                                mix(self.border_color_1_focus, self.border_color_2_focus, gradient_border.y),
                                self.focus
                            ),
                            mix(
                                mix(self.border_color_1_hover, self.border_color_2_hover, gradient_border.y),
                                mix(self.border_color_1_down, self.border_color_2_down, gradient_border.y),
                                self.down
                            ),
                            self.hover
                        ),
                        mix(self.border_color_1_disabled, self.border_color_2_disabled, gradient_border.y),
                        self.disabled
                    ),
                    self.border_size
                );
                
                return sdf.result
            }
        }

        draw_selection: {
            fn pixel(self) -> vec4 {
                let sdf = Sdf2d::viewport(self.pos * self.rect_size);

                sdf.box(
                    0.0,
                    0.0,
                    self.rect_size.x,
                    self.rect_size.y,
                    self.border_radius
                )

                sdf.fill(
                    mix(
                        mix(
                            mix(
                                mix(
                                    mix(self.color_1, self.color_2, self.pos.y),
                                    mix(self.color_1_empty, self.color_2_empty, self.pos.y),
                                    self.empty
                                ),
                                mix(self.color_1_focus, self.color_2_focus, self.pos.y),
                                self.focus
                            ),
                            mix(
                                mix(self.color_1_hover, self.color_2_hover, self.pos.y),
                                mix(self.color_1_down, self.color_2_down, self.pos.y),
                                self.down
                            ),
                            self.hover
                        ),
                        mix(self.color_1_disabled, self.color_2_disabled, self.pos.y),
                        self.disabled
                    )
                );

                return sdf.result
            }
        }
    }
}

#[derive(PartialEq)]
struct TextInputLayoutMetrics {
    font_family: crate::makepad_draw::shader::draw_text::FontFamily,
    font_size: f32,
    line_spacing: f32,
    font_scale: f32,
    max_width: Option<f32>,
    wrap: bool,
    align_x: f64,
    align_y: f64,
}

#[derive(Live, Widget)]
pub struct TextInput {
    #[animator] animator: Animator,

    #[redraw] #[live] draw_bg: DrawColor,
    #[live] draw_text: DrawText,
    #[live] draw_selection: DrawQuad,
    #[live] draw_cursor: DrawQuad,

    #[layout] layout: Layout,
    #[walk] walk: Walk,
    #[live] label_align: Align,

    #[live] is_password: bool,
    #[live] is_read_only: bool,
    #[live] is_numeric_only: bool,
    #[live] empty_text: String,
    #[rust] text: String,
    #[live(0.5)] blink_speed: f64,

    #[rust] password_text: String,
    #[rust] laidout_text: Option<Rc<LaidoutText>>,
    #[rust] layout_metrics: Option<TextInputLayoutMetrics>,
    #[rust] text_area: Area,
    #[rust] selection: Selection,
    #[rust] history: History,
    #[rust] blink_timer: Timer,
    #[rust] ime_composing: bool,
    #[rust] touch_selection: Option<DVec2>,
    #[rust] touch_focus: Option<DigitId>,
    #[rust] selection_handles: Option<crate::TextSelectionHandles>,
    #[rust] touch_handles: bool,
    #[rust] text_scroll: DVec2,
    #[rust] text_scroll_max: DVec2,
    #[rust(true)] reveal_cursor: bool,
}

 impl LiveHook for TextInput{
     fn apply_value_unknown(&mut self, cx: &mut Cx, apply: &mut Apply, index: usize, nodes: &[LiveNode]) -> usize {
        if nodes[index].id == live_id!(text){
            if !apply.from.is_update_from_doc(){
                return self.text.apply(cx, apply, index, nodes)
            }
        }
        else{
            cx.apply_error_no_matching_field(live_error_origin!(), index, nodes);
        }
        nodes.skip_node(index)
     }
     fn after_new_from_doc(&mut self, cx:&mut Cx){
         self.check_text_is_empty(cx);
     }
 }

impl TextInput {
    pub fn is_password(&self) -> bool {
        self.is_password
    }

    pub fn set_is_password(&mut self, cx: &mut Cx, is_password: bool) {
        self.is_password = is_password;
        self.laidout_text = None;
        self.draw_bg.redraw(cx);
    }

    pub fn toggle_is_password(&mut self, cx: &mut Cx) {
        self.set_is_password(cx, !self.is_password);
    }

    pub fn is_read_only(&self) -> bool {
        self.is_read_only
    }

    pub fn set_is_read_only(&mut self, cx: &mut Cx, is_read_only: bool) {
        self.is_read_only = is_read_only;
        self.laidout_text = None;
        self.draw_bg.redraw(cx);
    }

    pub fn toggle_is_read_only(&mut self, cx: &mut Cx) {
        self.set_is_read_only(cx, !self.is_read_only);
    }

    pub fn is_numeric_only(&self) -> bool {
        self.is_numeric_only
    }

    pub fn set_is_numeric_only(&mut self, cx: &mut Cx, is_numeric_only: bool) {
        self.is_numeric_only = is_numeric_only;
        self.laidout_text = None;
        self.draw_bg.redraw(cx);
    }

    pub fn toggle_is_numeric_only(&mut self, cx: &mut Cx) {
        self.set_is_numeric_only(cx, !self.is_numeric_only);
    }

    pub fn empty_text(&self) -> &str {
        &self.empty_text
    }

    pub fn set_empty_text(&mut self, cx: &mut Cx, empty_text: String) {
        self.empty_text = empty_text;
        if self.text.is_empty() {
            self.draw_bg.redraw(cx);
        }
    }


    pub fn selection(&self) -> Selection {
        self.selection
    }

    pub fn set_selection(&mut self, cx: &mut Cx, selection: Selection) {
        self.touch_focus = None;
        self.selection = selection;
        self.reveal_cursor = true;
        let area = self.draw_bg.area();
        if cx.has_key_focus(area) {
            if self.touch_handles || self.selection.anchor.index != self.selection.cursor.index {
                cx.claim_text_selection(area);
            } else if !cx.text_actions_active(area) {
                cx.clear_text_selection(area);
            }
        }
        self.history.force_new_edit_group();
        self.draw_bg.redraw(cx);
    }

    pub fn cursor(&self) -> Cursor {
        self.selection.cursor
    }

    pub fn set_cursor(&mut self, cx: &mut Cx, cursor: Cursor, keep_selection: bool) {
        self.set_selection(
            cx,
            Selection {
                anchor: if keep_selection {
                    self.selection.anchor
                } else {
                    cursor
                },
                cursor
            }
        );
    }
    
    pub fn selected_text(&self) -> &str {
        &self.text[self.selection.start().index..self.selection.end().index]
    }

    fn text_action_capabilities(&self) -> TextActionCapabilities {
        let selected = self.selection.anchor.index != self.selection.cursor.index;
        TextActionCapabilities {
            copy: selected && !self.is_password,
            cut: selected && !self.is_password && !self.is_read_only,
            paste: !self.is_read_only,
            select_all: !self.text.is_empty(),
        }
    }

    fn open_text_actions(&mut self, cx: &mut Cx) {
        if !cfg!(target_os = "android") { return; }
        let area = self.draw_bg.area();
        // Keep the source area until parent layout finalizes its clip and alignment.
        cx.show_text_actions_for_area(area, area, self.text_action_capabilities());
        self.draw_bg.redraw(cx);
    }

    // Flutter animates cursor opacity on Apple platforms only. Cutting the same
    // animator track elsewhere also updates its state, without requesting fade frames.
    fn set_blink_state(&mut self, cx: &mut Cx, state: &[LiveId; 2]) {
        if cfg!(any(target_os = "macos", target_os = "ios")) {
            self.animator_play(cx, state);
        } else {
            self.animator_cut(cx, state);
            self.draw_bg.redraw(cx);
        }
    }
    pub fn reset_blink_timer(&mut self, cx: &mut Cx) {
        self.animator_cut(cx, id!(blink.off));
        if !self.is_read_only {
            cx.stop_timer(self.blink_timer);
            self.blink_timer = cx.start_timeout(self.blink_speed)
        }
    }

    /// Release a retained editor even when its last draw area is no longer valid.
    /// A newer editor's pending focus and IME ownership are left untouched.
    pub fn release_focus(&mut self, cx: &mut Cx) {
        let released = cx.clear_key_focus_if(self.draw_bg.area());
        self.end_focus(cx, released);
    }

    fn end_focus(&mut self, cx: &mut Cx, hide_ime: bool) {
        self.touch_handles = false;
        if let Some(handles) = &mut self.selection_handles { handles.cancel(cx); }
        self.ime_composing = false;
        self.touch_selection = None;
        self.touch_focus = None;
        self.selection.anchor = self.selection.cursor;
        cx.clear_text_selection(self.draw_bg.area());
        self.history.force_new_edit_group();
        cx.clear_text_input(self.draw_bg.area());
        self.animator_cut(cx, id!(focus.off));
        self.animator_cut(cx, id!(blink.on));
        cx.stop_timer(self.blink_timer);
        self.blink_timer = Timer::empty();
        #[cfg(not(target_os = "android"))]
        if hide_ime {
            cx.hide_text_ime();
        }
        #[cfg(target_os = "android")]
        let _ = hide_ime;
    }

    fn cursor_to_position(&self, cursor: Cursor) -> Result<CursorPosition, ()> {
        let Some(laidout_text) = self.laidout_text.as_ref() else {
            return Err(());
        };
        let position = laidout_text.cursor_to_position(self.cursor_to_password_cursor(cursor));
        Ok(CursorPosition {
            row_index: position.row_index,
            x_in_lpxs: position.x_in_lpxs * self.draw_text.font_scale,
        })
    }

    fn point_in_lpxs_to_cursor(&self, point_in_lpxs: Point<f32>) -> Result<Cursor, ()> {
        let Some(laidout_text) = self.laidout_text.as_ref() else {
            return Err(());
        };
        let cursor = laidout_text.point_in_lpxs_to_cursor(point_in_lpxs / self.draw_text.font_scale);
        Ok(self.password_cursor_to_cursor(cursor))
    }

    fn position_to_cursor(&self, position: CursorPosition) -> Result<Cursor, ()> {
        let Some(laidout_text) = self.laidout_text.as_ref() else {
            return Err(());
        };
        let cursor = laidout_text.position_to_cursor(CursorPosition {
            row_index: position.row_index,
            x_in_lpxs: position.x_in_lpxs / self.draw_text.font_scale,
        });
        Ok(self.password_cursor_to_cursor(cursor))
    }

    fn selection_to_password_selection(&self, selection: Selection) -> Selection {
        Selection {
            cursor: self.cursor_to_password_cursor(selection.cursor),
            anchor: self.cursor_to_password_cursor(selection.anchor),
        }
    }

    fn cursor_to_password_cursor(&self, cursor: Cursor) -> Cursor {
        Cursor {
            index: self.index_to_password_index(cursor.index),
            prefer_next_row: cursor.prefer_next_row,
        }
    }

    fn password_cursor_to_cursor(&self, password_cursor: Cursor) -> Cursor {
        Cursor {
            index: self.password_index_to_index(password_cursor.index),
            prefer_next_row: password_cursor.prefer_next_row,
        }
    }

    fn index_to_password_index(&self, index: usize) -> usize {
        if !self.is_password {
            return index;
        }
        let grapheme_index = self.text[..index].graphemes(true).count();
        self.password_text
            .grapheme_indices(true)
            .nth(grapheme_index).map_or(self.password_text.len(), |(index, _)| index)
    }

    fn password_index_to_index(&self, password_index: usize) -> usize {
        if !self.is_password {
            return password_index;
        }
        let grapheme_index = self.password_text[..password_index].graphemes(true).count();
        self.text
            .grapheme_indices(true)
            .nth(grapheme_index).map_or(self.text.len(), |(index, _)| index)
    }

    fn inner_walk(&self) -> Walk {
        if self.walk.width.is_fit() {
            Walk::fit()
        } else {
            Walk::fill_fit()
        }
    }

    fn layout_text(&mut self, cx: &mut Cx2d) {
        let turtle_rect = cx.turtle().padded_rect();
        let max_width_in_lpxs = if !turtle_rect.size.x.is_nan() {
            Some(turtle_rect.size.x as f32)
        } else {
            None
        };
        let wrap = cx.turtle().layout().flow == Flow::RightWrap;
        let metrics = TextInputLayoutMetrics {
            font_family: self.draw_text.text_style.font_family.clone(),
            font_size: self.draw_text.text_style.font_size,
            line_spacing: self.draw_text.text_style.line_spacing,
            font_scale: self.draw_text.font_scale,
            max_width: max_width_in_lpxs,
            wrap,
            align_x: self.label_align.x,
            align_y: self.label_align.y,
        };
        if self.laidout_text.is_some() && self.layout_metrics.as_ref() == Some(&metrics) {
            return;
        }
        self.layout_metrics = Some(metrics);
        let text = if self.is_password {
            self.password_text.clear();
            for grapheme in self.text.graphemes(true) {
                self.password_text.push(if grapheme == "\n" {
                    '\n'
                } else {
                    '•'
                });
            }
            &self.password_text
        } else {
            &self.text
        };
        self.laidout_text = Some(self.draw_text.layout(
            cx,
            0.0,
            0.0,
            max_width_in_lpxs,
            wrap,
            self.label_align, 
            text
        ));
    }

    fn draw_text(&mut self, cx: &mut Cx2d) -> Rect {
        let inner_walk = self.inner_walk();
        let text_rect = if self.text.is_empty() {
            self.text_scroll = DVec2::default();
            self.text_scroll_max = DVec2::default();
            self.draw_text.draw_walk(
                cx,
                inner_walk,
                self.label_align,
                &self.empty_text
            )
        } else {
            let laidout_text = self.laidout_text.as_ref().unwrap();
            let size = laidout_text.size_in_lpxs * self.draw_text.font_scale;
            let viewport = cx.turtle().padded_rect();
            let width = if viewport.size.x.is_finite() { viewport.size.x.max(0.0) } else { size.width as f64 };
            let height = if viewport.size.y.is_finite() { viewport.size.y.max(0.0) } else { size.height as f64 };
            self.text_scroll_max = dvec2((size.width as f64 - width).max(0.0), (size.height as f64 - height).max(0.0));
            self.text_scroll.x = self.text_scroll.x.clamp(0.0, self.text_scroll_max.x);
            self.text_scroll.y = self.text_scroll.y.clamp(0.0, self.text_scroll_max.y);
            if self.reveal_cursor && cx.has_key_focus(self.draw_bg.area()) && !self.selection_handles.as_ref().is_some_and(|h| h.is_dragging()) {
                let caret = self.caret_rect(self.selection.cursor, DVec2::default());
                if caret.pos.x < self.text_scroll.x { self.text_scroll.x = caret.pos.x.max(0.0); }
                if caret.pos.x + caret.size.x > self.text_scroll.x + width {
                    self.text_scroll.x = (caret.pos.x + caret.size.x - width).clamp(0.0, self.text_scroll_max.x);
                }
                if caret.pos.y < self.text_scroll.y { self.text_scroll.y = caret.pos.y.max(0.0); }
                if caret.pos.y + caret.size.y > self.text_scroll.y + height {
                    self.text_scroll.y = (caret.pos.y + caret.size.y - height).clamp(0.0, self.text_scroll_max.y);
                }
            }
            self.reveal_cursor = false;
            let walk_rect = cx.walk_turtle(Walk {
                width: Size::Fixed(cx.turtle().max_width(inner_walk).unwrap_or(size.width as f64)),
                height: Size::Fixed(cx.turtle().max_height(inner_walk).unwrap_or(size.height as f64)),
                ..inner_walk
            });
            let pos = walk_rect.pos - self.text_scroll;
            self.draw_text.draw_abs_laidout(cx, pos, laidout_text);
            Rect { pos, size: dvec2(size.width as f64, size.height as f64) }
        };
        cx.add_aligned_rect_area(&mut self.text_area, text_rect);
        text_rect
    }

    fn caret_rect(&self, cursor: Cursor, origin: DVec2) -> Rect {
        let Some(layout) = self.laidout_text.as_ref() else { return Rect::default(); };
        let Ok(position) = self.cursor_to_position(cursor) else { return Rect::default(); };
        let row = &layout.rows[position.row_index];
        Rect {
            pos: origin + dvec2(position.x_in_lpxs as f64, ((row.origin_in_lpxs.y - row.ascender_in_lpxs) * self.draw_text.font_scale) as f64),
            size: dvec2(2.0, ((row.ascender_in_lpxs - row.descender_in_lpxs) * self.draw_text.font_scale) as f64),
        }
    }

    /// Draws the caret and returns its absolute rect.
    fn draw_cursor(&mut self, cx: &mut Cx2d, text_rect: Rect) -> Rect {
        let CursorPosition {
            row_index,
            x_in_lpxs,
        } = self
            .cursor_to_position(self.selection.cursor)
            .ok()
            .expect("layout should not be `None` because we called `layout_text` in `draw_walk`");
        let laidout_text = self
            .laidout_text
            .as_ref()
            .expect("layout should not be `None` because we called `layout_text` in `draw_walk`");
        let row = &laidout_text.rows[row_index];
        let caret = rect(
            text_rect.pos.x + (x_in_lpxs - 1.0 * self.draw_text.font_scale) as f64,
            text_rect.pos.y + ((row.origin_in_lpxs.y - row.ascender_in_lpxs) * self.draw_text.font_scale) as f64,
            (2.0 * self.draw_text.font_scale) as f64,
            ((row.ascender_in_lpxs - row.descender_in_lpxs) * self.draw_text.font_scale) as f64,
        );
        self.draw_cursor.draw_abs(cx, caret);
        caret
    }

    fn draw_selection(&mut self, cx: &mut Cx2d, text_rect: Rect) {
        let laidout_text = self
            .laidout_text
            .as_ref()
            .expect("layout should not be `None` because we called `layout_text` in `draw_walk`");
        
        self.draw_selection.begin_many_instances(cx);
        for rect_in_lpxs in laidout_text.selection_rects_in_lpxs(
            self.selection_to_password_selection(self.selection)
        ) {
            let selected = rect(
                text_rect.pos.x + (rect_in_lpxs.origin.x * self.draw_text.font_scale) as f64,
                text_rect.pos.y + (rect_in_lpxs.origin.y * self.draw_text.font_scale) as f64,
                (rect_in_lpxs.size.width * self.draw_text.font_scale) as f64,
                (rect_in_lpxs.size.height * self.draw_text.font_scale) as f64,
            );
            self.draw_selection.draw_abs(cx, selected);
        }
        self.draw_selection.end_many_instances(cx);
    }

    pub fn move_cursor_left(&mut self, cx: &mut Cx, keep_selection: bool) {
        self.set_cursor(
            cx,
            Cursor {
                index: prev_grapheme_boundary(&self.text, self.selection.cursor.index),
                prefer_next_row: true,
            },
            keep_selection
        );
    }

    pub fn move_cursor_right(&mut self, cx: &mut Cx, keep_selection: bool) {
        self.set_cursor(
            cx,
            Cursor {
                index: next_grapheme_boundary(&self.text, self.selection.cursor.index),
                prefer_next_row: false,
            },
            keep_selection,
        );
    }

    pub fn move_cursor_up(&mut self, cx: &mut Cx, keep_selection: bool) -> Result<(), ()> {
        let position = self.cursor_to_position(self.selection.cursor)?;
        self.set_cursor(
            cx,
            self.position_to_cursor(CursorPosition {
                row_index: if position.row_index == 0 {
                    0
                } else {
                    position.row_index - 1
                },
                x_in_lpxs: position.x_in_lpxs,
            })?,
            keep_selection
        );
        Ok(())
    }

    pub fn move_cursor_down(&mut self, cx: &mut Cx, keep_selection: bool) -> Result<(), ()> {
        let laidout_text = self.laidout_text.as_ref().unwrap();
        let position = self.cursor_to_position(self.selection.cursor)?;
        self.set_cursor(
            cx,
            self.position_to_cursor(CursorPosition {
                row_index: if position.row_index == laidout_text.rows.len() - 1 {
                    laidout_text.rows.len() - 1
                } else {
                    position.row_index + 1 
                },
                x_in_lpxs: position.x_in_lpxs,
            })?,
            keep_selection
        );
        Ok(())
    }

    pub fn select_all(&mut self, cx: &mut Cx) {
        self.set_selection(
            cx,
            Selection {
                anchor: Cursor { index: 0, prefer_next_row: false },
                cursor: Cursor { index: self.text.len(), prefer_next_row: false },
            }
        );
    }

    pub fn select_word(&mut self, cx: &mut Cx) {
        if self.selection.cursor.index < self.selection.anchor.index { 
            self.set_cursor(
                cx, 
                Cursor {
                    index: self.ceil_word_boundary(self.selection.cursor.index),
                    prefer_next_row: true,
                },
                true,
            );
        } else if self.selection.cursor.index > self.selection.anchor.index {
            self.set_cursor(
                cx,
                Cursor {
                    index: self.floor_word_boundary(self.selection.cursor.index),
                    prefer_next_row: false,
                },
                true,
            );
        } else {
            let anchor = self.ceil_word_boundary(self.selection.cursor.index);
            let cursor = self.floor_word_boundary(self.selection.cursor.index);
            // A press on the empty line after a trailing line break finds no word: the
            // segment before the end of the text is the break itself. Selecting that
            // invisible range leaves the text actions nothing to anchor to (no Paste on
            // the last line), so keep the caret there instead, like a platform editor.
            if self.text[anchor..cursor].chars().all(|c| c == '\n' || c == '\r') {
                return;
            }
            self.set_selection(
                cx,
                Selection {
                    anchor: Cursor { index: anchor, prefer_next_row: true },
                    cursor: Cursor { index: cursor, prefer_next_row: false },
                },
            );
        }
    }

    pub fn force_new_edit_group(&mut self) {
        self.history.force_new_edit_group();
    }

    fn ceil_word_boundary(&self, index: usize) -> usize {
        let mut prev_word_boundary_index = 0;
        for (word_boundary_index, _) in self.text.split_word_bound_indices() {
            if word_boundary_index > index {
                return prev_word_boundary_index;
            }
            prev_word_boundary_index = word_boundary_index;
        }
        prev_word_boundary_index
    }

    fn floor_word_boundary(&self, index: usize) -> usize {
        let mut prev_word_boundary_index = self.text.len();
        for (word_boundary_index, _) in self.text.split_word_bound_indices().rev() {
            if word_boundary_index < index {
                return prev_word_boundary_index;
            }
            prev_word_boundary_index = word_boundary_index;
        }
        prev_word_boundary_index
    }

    fn filter_input(&self, input: &str, is_set_text: bool) -> String {
        if self.is_numeric_only {
            let mut contains_dot = if is_set_text {
                false   
            } else {
                let before_selection = self.text[..self.selection.start().index].to_string();
                let after_selection = self.text[self.selection.end().index..].to_string();
                before_selection.contains('.') || after_selection.contains('.')
            };
            input.chars().filter(|char| {
                match char {
                    '.' | ',' if !contains_dot => {
                        contains_dot = true;
                        true
                    },
                    char => char.is_ascii_digit(),
                }
            }).collect()
        } else {
            input.to_string()
        }
    }

    fn create_or_extend_edit_group(&mut self, edit_kind: EditKind) {
        self.history.create_or_extend_edit_group(edit_kind, self.selection);
    }

    fn apply_edit(&mut self, cx: &mut Cx, edit: Edit) {
        self.reveal_cursor = true;
        self.selection.cursor.index = edit.start + edit.replace_with.len();
        self.selection.anchor.index = self.selection.cursor.index;
        self.history.apply_edit(edit, &mut self.text);
        self.laidout_text = None;
        self.check_text_is_empty(cx);
    }

    fn undo(&mut self, cx: &mut Cx) -> bool {
        if let Some(new_selection) = self.history.undo(self.selection, &mut self.text) {
            self.laidout_text = None;
            self.selection = new_selection;
            self.reveal_cursor = true;
            self.check_text_is_empty(cx);
            true
        } else {
            false
        }
    }

    fn redo(&mut self, cx: &mut Cx) -> bool {
        if let Some(new_selection) = self.history.redo(self.selection, &mut self.text) {
            self.laidout_text = None;
            self.selection = new_selection;
            self.reveal_cursor = true;
            self.check_text_is_empty(cx);
            true
        } else {
            false
        }
    }

    fn touch_focus_hit(&mut self, hit: &Hit) -> bool {
        match hit {
            Hit::FingerDown(fe) if fe.device.is_primary_hit() => {
                self.touch_focus = fe.device.is_touch().then_some(fe.digit_id);
            }
            Hit::FingerMove(fe) if fe.device.is_touch() && !fe.is_over => {
                self.touch_focus = None;
            }
            Hit::FingerUp(fe) => {
                return self.touch_focus.take() == Some(fe.digit_id) && fe.is_over && fe.was_tap();
            }
            Hit::FingerLongPress(fe) => {
                return self.touch_focus.take() == Some(fe.digit_id) && fe.rect.contains(fe.abs);
            }
            _ => {}
        }
        false
    }

    fn place_pointer_cursor(&mut self, cx: &mut Cx, abs: DVec2, tap_count: u32, keep_selection: bool) {
        let rel = abs - self.text_area.rect(cx).pos;
        let Ok(cursor) = self.point_in_lpxs_to_cursor(Point::new(rel.x as f32, rel.y as f32)) else {
            warning!("can't move cursor because layout was invalidated by earlier event");
            return;
        };
        self.set_cursor(cx, cursor, keep_selection);
        match tap_count {
            2 => self.select_word(cx),
            3 => self.select_all(cx),
            _ => {}
        }
    }

    fn check_text_is_empty(&mut self, cx: &mut Cx) {
        if self.text.is_empty() {
            self.animator_play(cx, id!(empty.on));
        } else {
            self.animator_play(cx, id!(empty.off));
        }
    }
    
}

impl Widget for TextInput {
        
    fn text(&self) -> String {
        self.text.clone()
    }
    
    fn set_text(&mut self, cx: &mut Cx, text: &str) {
        self.touch_focus = None;
        self.touch_selection = None;
        self.touch_handles = false;
        if let Some(handles) = &mut self.selection_handles { handles.cancel(cx); }
        cx.clear_text_selection(self.draw_bg.area());
        self.text = self.filter_input(text, true);
        self.set_selection(
            cx,
            Selection {
                anchor: Cursor {
                    index: floor_char_boundary(&self.text, self.selection.anchor.index),
                    prefer_next_row: self.selection.anchor.prefer_next_row,
                },
                cursor: Cursor {
                    index: floor_char_boundary(&self.text, self.selection.cursor.index),
                    prefer_next_row: self.selection.cursor.prefer_next_row,
                }
            }
        );
        self.history.clear();
        self.laidout_text = None;
        self.draw_bg.redraw(cx);
        self.check_text_is_empty(cx);
    }
    
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        // A different field's background uniforms (for example its focus border)
        // can start a newer draw call while our text still matches an older one.
        // Keep each input's background, selection, text and cursor in its own
        // batch group so that their draw order cannot cross another input.
        let draw_call_group = LiveId(self.widget_uid().0);
        self.draw_bg.draw_vars.set_draw_call_group(draw_call_group);
        self.draw_selection.draw_vars.set_draw_call_group(draw_call_group);
        self.draw_text.draw_vars.set_draw_call_group(draw_call_group);
        self.draw_cursor.draw_vars.set_draw_call_group(draw_call_group);
        self.draw_bg.begin(cx, walk, self.layout);
        self.draw_selection.append_to_draw_call(cx);
        self.layout_text(cx);
        let mut handle_viewport = cx.turtle().padded_rect();
        let text_rect = self.draw_text(cx);
        let caret = self.draw_cursor(cx, text_rect);
        self.draw_selection(cx, text_rect);
        self.draw_bg.end(cx);
        if !handle_viewport.size.x.is_finite() { handle_viewport.size.x = text_rect.size.x.max(1.0); }
        if !handle_viewport.size.y.is_finite() { handle_viewport.size.y = text_rect.size.y.max(1.0); }
        if cx.has_key_focus(self.draw_bg.area()) && !self.is_read_only {
            cx.update_text_input(self.draw_bg.area(), self.widget_uid().0, &self.text,
                self.selection.anchor.index, self.selection.cursor.index,
                self.is_password, self.is_numeric_only);
            // Desktop input methods draw the composition at the caret, at its height.
            let bg = self.draw_bg.area().rect(cx);
            cx.show_text_ime_caret(
                self.draw_bg.area(),
                Rect { pos: caret.pos - bg.pos, size: caret.size },
            );
        }
        let area = self.draw_bg.area();
        if cx.has_key_focus(area) {
            if self.touch_handles || self.selection.anchor.index != self.selection.cursor.index {
                cx.claim_text_selection(area);
            } else if !cx.text_actions_active(area) {
                cx.clear_text_selection(area);
            }
            if cx.text_actions_active(area) && !self.selection_handles.as_ref().is_some_and(|h| h.is_dragging()) {
                let anchor = if self.selection.anchor.index == self.selection.cursor.index {
                    self.draw_cursor.area()
                } else {
                    self.draw_selection.area()
                };
                cx.show_text_actions_for_area(area, anchor, self.text_action_capabilities());
            }
        }
        if self.touch_handles && cx.has_key_focus(area) {
            let anchor = self.caret_rect(self.selection.anchor, self.text_area.rect(cx).pos);
            let cursor = self.caret_rect(self.selection.cursor, self.text_area.rect(cx).pos);
            self.selection_handles.get_or_insert_with(|| crate::TextSelectionHandles::new(cx))
                .draw(cx, area, handle_viewport, anchor, cursor,
                    self.selection.anchor.index == self.selection.cursor.index, !self.is_read_only);
        } else if let Some(handles) = &mut self.selection_handles {
            handles.cancel(cx);
        }
        cx.add_nav_stop(self.draw_bg.area(), NavRole::TextInput, Margin::default());
        DrawStep::done()
    }

    fn set_disabled(&mut self, cx:&mut Cx, disabled:bool){
        if disabled {
            self.touch_handles = false;
            if let Some(handles) = &mut self.selection_handles { handles.cancel(cx); }
        }
        self.animator_toggle(cx, disabled, Animate::Yes, id!(disabled.on), id!(disabled.off));
    }
                
    fn disabled(&self, cx:&Cx) -> bool {
        self.animator_in_state(cx, id!(disabled.on))
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let area = self.draw_bg.area();
        let handle_action = self.selection_handles.as_mut().and_then(|handles| handles.handle_event(cx, event, area));
        if let Some(action) = handle_action {
            self.reveal_cursor = false;
            self.touch_focus = None;
            self.touch_selection = None;
            if action.phase != TextHandlePhase::Cancel {
                if action.phase == TextHandlePhase::Start {
                    self.ime_composing = false;
                    self.history.force_new_edit_group();
                }
                let before = self.text_scroll;
                self.text_scroll.x = (before.x + action.scroll.x).clamp(0.0, self.text_scroll_max.x);
                self.text_scroll.y = (before.y + action.scroll.y).clamp(0.0, self.text_scroll_max.y);
                let rel = action.abs - self.text_area.rect(cx).pos + self.text_scroll - before;
                if let Ok(cursor) = self.point_in_lpxs_to_cursor(Point::new(rel.x as f32, rel.y as f32)) {
                    match action.endpoint {
                        TextHandleEndpoint::Caret => self.selection = Selection { anchor: cursor, cursor },
                        TextHandleEndpoint::Anchor => self.selection.anchor = cursor,
                        TextHandleEndpoint::Cursor => self.selection.cursor = cursor,
                    }
                }
                self.reset_blink_timer(cx);
                if !self.is_read_only {
                    cx.update_text_input(area, self.widget_uid().0, &self.text,
                        self.selection.anchor.index, self.selection.cursor.index, self.is_password, self.is_numeric_only);
                }
                if action.phase == TextHandlePhase::End { self.open_text_actions(cx); }
            }
            self.draw_bg.redraw(cx);
            return;
        }
        if matches!(event, Event::KeyFocus(_)) {
            self.touch_focus = None;
        }
        if matches!(event, Event::TouchUpdate(update) if update.cancelled) {
            self.touch_focus = None;
            self.touch_selection = None;
        }
        if self.animator_handle_event(cx, event).must_redraw() {
            self.draw_bg.redraw(cx);
        }

        if self.blink_timer.is_event(event).is_some() {
            if self.animator_in_state(cx, id!(blink.off)) {
                self.set_blink_state(cx, id!(blink.on));
            } else {
                self.set_blink_state(cx, id!(blink.off));
            }
            self.blink_timer = cx.start_timeout(self.blink_speed)
        }

        let uid = self.widget_uid();
        let hit = event.hits(cx, self.draw_bg.area());
        let touch_focus = self.touch_focus_hit(&hit);
        match hit {
            Hit::FingerHoverIn(_) => {
                self.animator_play(cx, id!(hover.on));
            }
            Hit::FingerHoverOut(_) => {
                self.animator_play(cx, id!(hover.off));
            }
            Hit::KeyFocus(_) => {
                self.ime_composing = false;
                self.animator_play(cx, id!(focus.on));
                self.reset_blink_timer(cx);
                cx.widget_action(uid, &scope.path, TextInputAction::KeyFocus);
            },
            Hit::KeyFocusLost(event) => {
                self.end_focus(cx, event.focus == Area::Empty);
                self.draw_bg.redraw(cx);
                cx.widget_action(uid, &scope.path, TextInputAction::KeyFocusLost);
            }
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::ArrowLeft,
                modifiers: KeyModifiers {
                    shift: keep_selection,
                    logo: false,
                    alt: false,
                    control: false
                },
                ..
            }) => {
                self.reset_blink_timer(cx);
                self.move_cursor_left(cx, keep_selection);
            }
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::ArrowRight,
                modifiers: KeyModifiers {
                    shift: keep_selection,
                    logo: false,
                    alt: false,
                    control: false
                },
                ..
            }) => {
                self.reset_blink_timer(cx);
                self.move_cursor_right(cx, keep_selection);
            }
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::ArrowUp,
                modifiers: KeyModifiers {
                    shift: keep_selection,
                    logo: false,
                    alt: false,
                    control: false
                },
                ..
            }) => {
                self.reset_blink_timer(cx);
                if self.move_cursor_up(cx, keep_selection).is_err() {
                    warning!("can't move cursor because layout was invalidated by earlier event");
                }
            },
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::ArrowDown,
                modifiers: KeyModifiers {
                    shift: keep_selection,
                    logo: false,
                    alt: false,
                    control: false
                },
                ..
            }) => {
                self.reset_blink_timer(cx);
                if self.move_cursor_down(cx, keep_selection).is_err() {
                    warning!("can't move cursor because layout was invalidated by earlier event");
                }
            }
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::KeyA,
                modifiers,
                ..
            }) if modifiers.is_primary() => self.select_all(cx),
            Hit::FingerDown(fe) if fe.device.is_primary_hit() => {
                self.touch_selection = None;
                // Touch owns neither focus nor the caret until release. In particular,
                // a pan through another input must leave the current IME untouched.
                if fe.device.is_touch() {
                    return;
                }
                self.touch_handles = false;
                if let Some(handles) = &mut self.selection_handles { handles.cancel(cx); }
                self.reset_blink_timer(cx);
                self.set_key_focus(cx);
                self.place_pointer_cursor(cx, fe.abs, fe.tap_count, false);
                if self.selection.anchor.index != self.selection.cursor.index {
                    cx.claim_text_selection(self.draw_bg.area());
                }

                self.animator_play(cx, id!(hover.down));
            }
            Hit::FingerLongPress(fe) if touch_focus => {
                // Selection is a distinct gesture, not a delayed ordinary tap.
                self.touch_handles = true;
                self.touch_selection = Some(fe.abs);
                self.set_key_focus(cx);
                self.reset_blink_timer(cx);
                self.place_pointer_cursor(cx, fe.abs, 2, false);
                cx.claim_text_selection(self.draw_bg.area());
                self.open_text_actions(cx);
            }
            Hit::FingerUp(fe) => {
                if touch_focus {
                    self.touch_handles = true;
                    self.reset_blink_timer(cx);
                    self.set_key_focus(cx);
                    self.place_pointer_cursor(cx, fe.abs, fe.tap_count, false);
                }
                let selected_by_touch = self.touch_selection.take().is_some()
                    || (touch_focus && self.selection.anchor.index != self.selection.cursor.index);
                if selected_by_touch {
                    self.open_text_actions(cx);
                }
                if fe.is_over && fe.was_tap() {
                    if fe.has_hovers() {
                        self.animator_play(cx, id!(hover.on));
                    } else {
                        self.animator_play(cx, id!(hover.off));
                    }
                } else {
                    self.animator_play(cx, id!(hover.off));
                }
            }
            Hit::FingerMove(fe) if fe.device.is_primary_hit() => {
                if fe.device.is_touch() {
                    let Some(previous) = self.touch_selection.as_mut() else { return; };
                    // IME reflow must not turn a stationary hold into a new selection.
                    if *previous == fe.abs { return; }
                    *previous = fe.abs;
                }
                self.reset_blink_timer(cx);
                if !fe.device.is_touch() {
                    self.set_key_focus(cx);
                }
                self.place_pointer_cursor(cx, fe.abs, fe.tap_count, true);
            }
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::ReturnKey,
                modifiers: mods @ KeyModifiers {
                    shift: false,
                    ..
                },
                ..
            }) => {
                cx.hide_text_ime();
                cx.widget_action(
                    uid,
                    &scope.path,
                    TextInputAction::Returned(
                        self.text.clone(),
                        mods,
                    ),
                );
            },

            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::Escape,
                ..
            }) => {
                cx.widget_action(uid, &scope.path, TextInputAction::Escaped);
            }
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::ReturnKey,
                modifiers: KeyModifiers {
                    shift: true,
                    ..
                },
                ..
            }) if !self.is_read_only => {
                self.reset_blink_timer(cx);
                self.create_or_extend_edit_group(EditKind::Other);
                self.apply_edit(
                    cx,
                    Edit {
                        start: self.selection.start().index,
                        end: self.selection.end().index,
                        replace_with: "\n".to_string(),
                    }
                );
                self.draw_bg.redraw(cx);
                cx.widget_action(uid, &scope.path, TextInputAction::Changed(self.text.clone()));
            }
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::Backspace,
                ..
            }) if !self.is_read_only => {
                self.reset_blink_timer(cx);
                let mut start = self.selection.start().index;
                let end = self.selection.end().index;
                if start == end {
                    start = prev_grapheme_boundary(&self.text, start);
                }
                self.create_or_extend_edit_group(EditKind::Backspace);
                self.apply_edit(
                    cx,
                    Edit {
                        start,
                        end,
                        replace_with: String::new(),
                    }
                );
                self.draw_bg.redraw(cx);
                cx.widget_action(uid, &scope.path, TextInputAction::Changed(self.text.clone()));
            }
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::Delete,
                ..
            }) if !self.is_read_only => {
                self.reset_blink_timer(cx);
                let start = self.selection.start().index;
                let mut end = self.selection.end().index;
                if start == end {
                    end = next_grapheme_boundary(&self.text, end);
                }
                self.create_or_extend_edit_group(EditKind::Delete);
                self.apply_edit(
                    cx,
                    Edit {
                        start,
                        end,
                        replace_with: String::new(),
                    }
                );
                self.draw_bg.redraw(cx);
                cx.widget_action(uid, &scope.path, TextInputAction::Changed(self.text.clone()));
            }
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::KeyZ,
                modifiers: modifiers @ KeyModifiers {
                    shift: false,
                    ..
                },
                ..
            }) if modifiers.is_primary() && !self.is_read_only => {
                if !self.undo(cx) {
                    return;
                }
                self.draw_bg.redraw(cx);
                cx.widget_action(uid, &scope.path, TextInputAction::Changed(self.text.clone()));
            }
            Hit::KeyDown(KeyEvent {
                key_code: KeyCode::KeyZ,
                modifiers: modifiers @ KeyModifiers {
                    shift: true,
                    ..
                },
                ..
            }) if modifiers.is_primary() && !self.is_read_only => {
                if !self.redo(cx) {
                    return;
                }
                self.draw_bg.redraw(cx);
                cx.widget_action(uid, &scope.path, TextInputAction::Changed(self.text.clone()));
            }
            Hit::TextIme(edit) if !self.is_read_only => {
                let changed = self.text != edit.text;
                let composition = self.ime_composing || edit.composing.is_some();
                if changed {
                    if !self.ime_composing && composition { self.history.force_new_edit_group(); }
                    let (start, end, replacement) = edit.replacement(&self.text);
                    let kind = if composition {
                        EditKind::Ime
                    } else if start == end && replacement.chars().count() == 1 {
                        EditKind::Insert
                    } else if replacement.is_empty() && self.selection.anchor.index == self.selection.cursor.index {
                        if end == self.selection.cursor.index { EditKind::Backspace } else { EditKind::Delete }
                    } else {
                        EditKind::Other
                    };
                    self.create_or_extend_edit_group(kind);
                    self.selection.anchor.index = start;
                    self.selection.cursor.index = end;
                    let input = self.filter_input(replacement, false);
                    self.apply_edit(cx, Edit { start, end, replace_with: input });
                }
                let (anchor, cursor) = edit.selection_utf8();
                self.selection.anchor.index = floor_char_boundary(&self.text, anchor);
                self.selection.cursor.index = floor_char_boundary(&self.text, cursor);
                self.ime_composing = edit.composing.is_some();
                self.reveal_cursor = true;
                if !self.ime_composing && (composition || !changed) { self.history.force_new_edit_group(); }
                self.reset_blink_timer(cx);
                self.draw_bg.redraw(cx);
                if changed {
                    cx.widget_action(uid, &scope.path, TextInputAction::Changed(self.text.clone()));
                }
            }
            Hit::TextInput(TextInputEvent {
                input,
                replace_last,
                was_paste,
                ..
            }) if !self.is_read_only => {
                let input = self.filter_input(&input, false);
                if input.is_empty() {
                    return;
                }
                self.create_or_extend_edit_group(
                    if replace_last || was_paste {
                        EditKind::Other
                    } else {
                        EditKind::Insert
                    }
                );
                self.apply_edit(
                    cx,
                    Edit {
                        start: self.selection.start().index,
                        end: self.selection.end().index,
                        replace_with: input
                    }
                );
                self.animator_play(cx, id!(empty.off));
                self.draw_bg.redraw(cx);
                cx.widget_action(uid, &scope.path, TextInputAction::Changed(self.text.clone()));
            }
            Hit::TextCopy(event) if !self.is_password => {
                *event.response.borrow_mut() = Some(self.selected_text().to_string());
            }
            Hit::TextCut(event) if !self.is_password && !self.is_read_only => {
                *event.response.borrow_mut() = Some(self.selected_text().to_string());
                if !self.selected_text().is_empty() {
                    self.history.create_or_extend_edit_group(EditKind::Other, self.selection);
                    self.apply_edit(
                        cx,
                        Edit {
                            start: self.selection.start().index,
                            end: self.selection.end().index,
                            replace_with: String::new(),
                        }
                    );
                    self.draw_bg.redraw(cx);
                    cx.widget_action(uid, &scope.path, TextInputAction::Changed(self.text.clone()));
                }
            }
            Hit::KeyDown(event) => {
                cx.widget_action(uid, &scope.path, TextInputAction::KeyDownUnhandled(event));
            }
            _ => {}
        }
        if cx.has_key_focus(self.draw_bg.area()) && !self.is_read_only {
            cx.update_text_input(self.draw_bg.area(), self.widget_uid().0, &self.text,
                self.selection.anchor.index, self.selection.cursor.index,
                self.is_password, self.is_numeric_only);
        }
    }
}

impl TextInputRef {
    pub fn release_focus(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.release_focus(cx);
        }
    }

    pub fn is_password(&self) -> bool {
        if let Some(inner) = self.borrow(){
            inner.is_password()
        }
        else{
            false
        }
    }
 
    pub fn set_is_password(&self, cx: &mut Cx, is_password: bool) {
        if let Some(mut inner) = self.borrow_mut(){
            inner.set_is_password(cx, is_password);
        }
    }
 
    pub fn toggle_is_password(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut(){
            inner.toggle_is_password(cx);
        }
    }

    pub fn is_read_only(&self) -> bool {
        if let Some(inner) = self.borrow(){
            inner.is_read_only()
        }
        else{
            false
        }
    }

    pub fn set_is_read_only(&self, cx: &mut Cx, is_read_only: bool) {
        if let Some(mut inner) = self.borrow_mut(){
            inner.set_is_read_only(cx, is_read_only);
        }
    }

    pub fn toggle_is_read_only(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut(){
            inner.toggle_is_read_only(cx);
        }
    }

    pub fn is_numeric_only(&self) -> bool {
        if let Some(inner) = self.borrow(){
            inner.is_numeric_only()
        }
        else{
            false
        }
    }

    pub fn set_is_numeric_only(&self, cx: &mut Cx, is_numeric_only: bool) {
        if let Some(mut inner) = self.borrow_mut(){
            inner.set_is_numeric_only(cx, is_numeric_only);
        }
    }

    pub fn toggle_is_numeric_only(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut(){
            inner.toggle_is_numeric_only(cx);
        }
    }

    pub fn empty_text(&self) -> String {
        if let Some(inner) = self.borrow(){
            inner.empty_text().to_string()
        }
        else{
            String::new()
        }
    }

    pub fn set_empty_text(&self, cx: &mut Cx, empty_text: String) {
        if let Some(mut inner) = self.borrow_mut(){
            inner.set_empty_text(cx, empty_text);
        }
    }

    pub fn selection(&self) -> Selection {
        if let Some(inner) = self.borrow(){
            inner.selection()
        }
        else{
            Default::default()
        }
    }

    pub fn set_selection(&self, cx: &mut Cx, selection: Selection) {
        if let Some(mut inner) = self.borrow_mut(){
            inner.set_selection(cx, selection);
        }
    }

    pub fn cursor(&self) -> Cursor {
        if let Some(inner) = self.borrow(){
            inner.cursor()
        }
        else{
            Default::default()
        }
    }

    pub fn set_cursor(&self, cx: &mut Cx, cursor: Cursor, keep_selection: bool) {
        if let Some(mut inner) = self.borrow_mut(){
            inner.set_cursor(cx, cursor, keep_selection);
        }
    }

    pub fn selected_text(&self) -> String {
        if let Some(inner) = self.borrow(){
            inner.selected_text().to_string()
        }
        else{
            String::new()
        }
    }

    pub fn returned(&self, actions: &Actions) -> Option<(String, KeyModifiers)> {
        for action in actions.filter_widget_actions_cast::<TextInputAction>(self.widget_uid()){
            if let TextInputAction::Returned(text, modifiers) = action {
                return Some((text, modifiers));
            }
        }
        None
    }
    
    pub fn escaped(&self, actions: &Actions) -> bool {
        for action in actions.filter_widget_actions_cast::<TextInputAction>(self.widget_uid()){
            if let TextInputAction::Escaped = action {
                return true;
            }
        }
        false
    }

    pub fn changed(&self, actions: &Actions) -> Option<String> {
        for action in actions.filter_widget_actions_cast::<TextInputAction>(self.widget_uid()){
            if let TextInputAction::Changed(text) = action{
                return Some(text);
            }
        }
        None
    }

    pub fn key_down_unhandled(&self, actions: &Actions) -> Option<KeyEvent> {
        for action in actions.filter_widget_actions_cast::<TextInputAction>(self.widget_uid()){
            if let TextInputAction::KeyDownUnhandled(event) = action{
                return Some(event);
            }
        }
        None
    }

    /// Saves the internal state of this text input widget
    /// to a new `TextInputState` object.
    pub fn save_state(&self) -> TextInputState {
        if let Some(inner) = self.borrow() {
            TextInputState {
                text: inner.text.clone(),
                password_text: inner.password_text.clone(),
                selection: inner.selection.clone(),
                history: inner.history.clone(),
            }
        } else {
            TextInputState::default()
        }
    }

    /// Restores the internal state of this text input widget
    /// from the given `TextInputState` object.
    pub fn restore_state(&self, cx: &mut Cx, state: TextInputState) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_text(cx, &state.text);
            inner.password_text = state.password_text;
            inner.history = state.history;
            inner.set_selection(cx, state.selection);
        }
    }
}

/// The saved (checkpointed) state of a text input widget.
#[derive(Clone, Debug, Default)]
pub struct TextInputState {
    text: String,
    password_text: String,
    selection: Selection,
    history: History,
}

#[derive(Clone, Debug, DefaultNone)]
pub enum TextInputAction {
    None,
    KeyFocus,
    KeyFocusLost,
    Returned(String, KeyModifiers),
    Escaped,
    Changed(String),
    KeyDownUnhandled(KeyEvent),
}

#[derive(Clone, Debug, Default)]
struct History {
    current_edit_kind: Option<EditKind>,
    undo_stack: EditStack,
    redo_stack: EditStack,
}

impl History {
    fn force_new_edit_group(&mut self) {
        self.current_edit_kind = None;
    }

    fn create_or_extend_edit_group(&mut self, edit_kind: EditKind, selection: Selection) {
        if !self.current_edit_kind.map_or(false, |current_edit_kind| current_edit_kind.can_merge_with(edit_kind)) {
            self.undo_stack.push_edit_group(selection);
            self.current_edit_kind = Some(edit_kind);
        }
    }

    fn apply_edit(&mut self, edit: Edit, text: &mut String) {
        let inverted_edit = edit.invert(&text);
        edit.apply(text);
        self.undo_stack.push_edit(inverted_edit);
        self.redo_stack.clear();
    }

    fn undo(
        &mut self,
        selection: Selection,
        text: &mut String,
    ) -> Option<Selection> {
        if let Some((new_selection, edits)) = self.undo_stack.pop_edit_group() {
            self.redo_stack.push_edit_group(selection);
            for edit in &edits {
                let inverted_edit = edit.invert(text);
                edit.apply(text);
                self.redo_stack.push_edit(inverted_edit);
            }
            self.current_edit_kind = None;
            Some(new_selection)
        } else {
            None
        }
    }

    fn redo(
        &mut self,
        selection: Selection,
        text: &mut String,
    ) -> Option<Selection> {
        if let Some((new_selection, edits)) = self.redo_stack.pop_edit_group() {
            self.undo_stack.push_edit_group(selection);
            for edit in &edits {
                let inverted_edit = edit.invert(text);
                edit.apply(text);
                self.undo_stack.push_edit(inverted_edit);
            }
            self.current_edit_kind = None;
            Some(new_selection)
        } else {
            None
        }
    }

    fn clear(&mut self) {
        self.current_edit_kind = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditKind {
    Insert,
    Backspace,
    Delete,
    Ime,
    Other,
}

impl EditKind {
    fn can_merge_with(self, other: EditKind) -> bool {
        if self == Self::Other {
            false
        } else {
            self == other
        }
    }
}

#[derive(Clone, Debug, Default)]
struct EditStack {
    edit_groups: Vec<EditGroup>,
    edits: Vec<Edit>,
}

impl EditStack {
    fn push_edit_group(&mut self, selection: Selection) {
        self.edit_groups.push(EditGroup {
            selection,
            edit_start: self.edits.len(),
        });
    }
    
    fn push_edit(&mut self, edit: Edit) {
        self.edits.push(edit);
    }
    
    fn pop_edit_group(&mut self) -> Option<(Selection, Vec<Edit>)> {
        match self.edit_groups.pop() {
            Some(edit_group) => Some((
                edit_group.selection,
                self.edits.drain(edit_group.edit_start..).rev().collect()
            )),
            None => None,
        }
    }
    
    fn clear(&mut self) {
        self.edit_groups.clear();
        self.edits.clear();
    }
}

#[derive(Clone, Copy, Debug)]
struct EditGroup {
    selection: Selection,
    edit_start: usize
}

#[derive(Clone, Debug)]
struct Edit {
    start: usize,
    end: usize,
    replace_with: String,
}

impl Edit {
    fn apply(&self, text: &mut String) {
        text.replace_range(self.start..self.end, &self.replace_with);
    }

    fn invert(&self, text: &str) -> Self {
        Self {
            start: self.start,
            end: self.start + self.replace_with.len(),
            replace_with: text[self.start..self.end].to_string(),
        }
    }
}

fn prev_grapheme_boundary(text: &str, index: usize) -> usize {
    let mut cursor = GraphemeCursor::new(index, text.len(), true);
    cursor.prev_boundary(text, 0).unwrap().unwrap_or(0)
}

fn next_grapheme_boundary(text: &str, index: usize) -> usize {
    let mut cursor = GraphemeCursor::new(index, text.len(), true);
    cursor.next_boundary(text, 0).unwrap().unwrap_or(text.len())
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) { index -= 1; }
    index
}

#[cfg(test)]
mod touch_focus_tests {
    use super::*;
    use crate::makepad_draw::event::{FingerLongPressEvent, TouchUpdateEvent};

    fn down() -> Hit {
        Hit::FingerDown(FingerDownEvent {
            window_id: WindowId(0, 0), abs: dvec2(5.0, 5.0),
            digit_id: live_id_num!(touch, 1).into(), device: DigitDevice::Touch { uid: 1 },
            tap_count: 1, modifiers: KeyModifiers::default(), time: 0.0, rect: Rect::default(),
        })
    }

    fn up() -> FingerUpEvent {
        FingerUpEvent {
            window_id: WindowId(0, 0), abs: dvec2(5.0, 5.0), abs_start: dvec2(5.0, 5.0),
            capture_time: 0.0, time: 0.1, digit_id: live_id_num!(touch, 1).into(),
            device: DigitDevice::Touch { uid: 1 }, has_long_press_occurred: false,
            tap_cancelled: false, tap_count: 1, modifiers: KeyModifiers::default(),
            rect: Rect::default(), is_over: true, is_sweep: false,
        }
    }

    #[test]
    fn input_focus_requires_its_own_uncancelled_inside_release() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut input = TextInput::new(&mut cx);
        assert!(!input.touch_focus_hit(&Hit::FingerUp(up())));
        for kind in 0..5 {
            assert!(!input.touch_focus_hit(&down()));
            let mut release = up();
            match kind {
                1 => release.is_over = false,
                2 => release.tap_cancelled = true,
                3 => release.has_long_press_occurred = true,
                4 => release.digit_id = live_id_num!(touch, 2).into(),
                _ => {}
            }
            assert_eq!(input.touch_focus_hit(&Hit::FingerUp(release)), kind == 0);
            assert!(!input.touch_focus_hit(&Hit::FingerUp(up())));
        }
    }

    #[test]
    fn leaving_input_or_changing_model_or_focus_invalidates_touch() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut input = TextInput::new(&mut cx);
        for transition in 0..4 {
            assert!(!input.touch_focus_hit(&down()));
            match transition {
                0 => {
                    input.touch_focus_hit(&Hit::FingerMove(FingerMoveEvent {
                        window_id: WindowId(0, 0), abs: dvec2(6.0, 5.0), abs_start: dvec2(5.0, 5.0),
                        digit_id: live_id_num!(touch, 1).into(), device: DigitDevice::Touch { uid: 1 },
                        tap_count: 1, modifiers: KeyModifiers::default(), time: 0.05,
                        rect: Rect::default(), is_over: false,
                    }));
                }
                1 => input.set_text(&mut cx, "replacement"),
                2 => input.handle_event(&mut cx, &Event::KeyFocus(KeyFocusEvent {
                    prev: Area::Empty, focus: Area::Empty,
                }), &mut Scope::empty()),
                _ => input.handle_event(&mut cx, &Event::TouchUpdate(TouchUpdateEvent {
                    window_id: WindowId(0, 0), time: 0.05, modifiers: KeyModifiers::default(),
                    touches: Vec::new(), cancelled: true,
                }), &mut Scope::empty()),
            }
            assert!(!input.touch_focus_hit(&Hit::FingerUp(up())));
        }
    }

    #[test]
    fn long_press_is_consumed_once_and_does_not_turn_into_a_tap() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut input = TextInput::new(&mut cx);
        input.touch_focus_hit(&down());
        let hold = Hit::FingerLongPress(FingerLongPressEvent {
            window_id: WindowId(0, 0), abs: dvec2(5.0, 5.0), capture_time: 0.0, time: 0.5,
            digit_id: live_id_num!(touch, 1).into(), device: DigitDevice::Touch { uid: 1 },
            rect: Rect { pos: dvec2(0.0, 0.0), size: dvec2(10.0, 10.0) },
        });
        assert!(input.touch_focus_hit(&hold));
        assert!(!input.touch_focus_hit(&hold));
        assert!(!input.touch_focus_hit(&Hit::FingerUp(up())));
    }

    #[test]
    fn word_selection_on_the_empty_last_line_keeps_the_caret() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut input = TextInput::new(&mut cx);
        let at = |input: &mut TextInput, cx: &mut Cx, text: &str, index: usize| {
            input.set_text(cx, text);
            input.set_cursor(cx, Cursor { index, prefer_next_row: false }, false);
            input.select_word(cx);
            (input.selection.anchor.index, input.selection.cursor.index)
        };
        // The only segment before the end is the trailing break: nothing to select, so the
        // text actions anchor to the caret and offer Paste.
        assert_eq!(at(&mut input, &mut cx, "abc\n", 4), (4, 4));
        assert_eq!(at(&mut input, &mut cx, "abc\r\n", 5), (5, 5));
        // A word still selects, including the last word of the text.
        assert_eq!(at(&mut input, &mut cx, "abc", 3), (0, 3));
        assert_eq!(at(&mut input, &mut cx, "abc def\n", 5), (4, 7));
    }
}