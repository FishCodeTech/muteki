#ifndef Tcode_BridgingHeader_h
#define Tcode_BridgingHeader_h

#include <stddef.h>
#include <stdint.h>

typedef struct GpuiIosTouch {
    uint64_t identifier;
    double x;
    double y;
    double predicted_x;
    double predicted_y;
    float force;
    uint8_t has_prediction;
    uint8_t has_force;
} GpuiIosTouch;

void tcode_ios_start(void);
void tcode_ios_speech_event(uint64_t id, uint32_t kind, const uint8_t *bytes, size_t length);
void tcode_ios_speech_availability_changed(void);
void tcode_ios_network_changed(void);
uint8_t tcode_ios_can_swipe_back(void);
void tcode_ios_edge_back(uint8_t phase, float travel, float velocity);
void tcode_ios_browse_found(uint64_t request, const uint8_t *id_bytes,
                            size_t id_length, const uint8_t *address_bytes,
                            size_t address_length);
void tcode_ios_camera_scan_completed(uint64_t request_id,
                                     const uint8_t *value_bytes,
                                     size_t value_length,
                                     const uint8_t *error_bytes,
                                     size_t error_length);
void tcode_ios_image_picked(uint64_t request_id, const uint8_t *name_bytes,
                            size_t name_length, const uint8_t *mime_bytes,
                            size_t mime_length, const uint8_t *data,
                            size_t data_length);
void tcode_ios_image_pick_finished(uint64_t request_id,
                                   const uint8_t *error_bytes,
                                   size_t error_length);

void gpui_ios_init(void);
uint8_t gpui_ios_attach_view(void *view, float width, float height, float scale,
                             uint8_t dark);
void gpui_ios_detach_view(void *view);
uint8_t gpui_ios_request_frame(void);
void gpui_ios_display_changed(double refresh_rate);

void gpui_ios_touches_began(const GpuiIosTouch *touches, size_t count);
void gpui_ios_touches_moved(const GpuiIosTouch *touches, size_t count);
void gpui_ios_touches_ended(const GpuiIosTouch *touches, size_t count);
void gpui_ios_touches_cancelled(const GpuiIosTouch *touches, size_t count);

void gpui_ios_resize(float width, float height, float scale);
void gpui_ios_scale_factor_changed(float scale);
void gpui_ios_safe_area_changed(float top, float right, float bottom,
                                float left);
void gpui_ios_keyboard_frame_changed(float height);
void gpui_ios_appearance_changed(uint8_t dark);

void gpui_ios_lifecycle_active(void);
void gpui_ios_lifecycle_inactive(void);
void gpui_ios_lifecycle_background(void);
void gpui_ios_lifecycle_foreground(void);
void gpui_ios_memory_warning(void);

typedef struct TextRange { size_t location; size_t length; } TextRange;
typedef struct TextRect { double x; double y; double width; double height; } TextRect;
TextRange gpui_ios_text_range(uint32_t kind);
size_t gpui_ios_read_text(TextRange range, uint8_t *bytes, size_t capacity);
void gpui_ios_replace_text(TextRange range, const uint8_t *bytes, size_t length,
                           uint8_t marked, TextRange selection);
void gpui_ios_select_text(TextRange range);
void gpui_ios_unmark_text(void);
TextRect gpui_ios_text_rect(TextRange range);
size_t gpui_ios_character_at_point(double x, double y);
void gpui_ios_move_caret_toward(double x, double y);
void gpui_ios_delete_backward(void);
void gpui_ios_paste_text(const uint8_t *bytes, size_t length);
uint8_t gpui_ios_key_event(const uint8_t *key_bytes, size_t key_length,
                        const uint8_t *character_bytes,
                        size_t character_length, uint32_t modifier_bits,
                        uint8_t down, uint8_t repeat);
void gpui_ios_open_url_received(const uint8_t *bytes, size_t length);

#endif
