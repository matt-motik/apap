
# описание разметки
top_panel_w = window_w
VSEP_W = GAP + | + GAP
col_cover_w = cover_wh
col_viz_w = top_panel_w - (col_cover_w + col_info_w + col_volume_w + VSEP_W*3)

top_panel_h = col_info_h,  но не больше (font_h  + GAP)* 14
control_panel_w = col_cover_w
control_button_wh = col_cover_w / (количество кнопок)

control_panel_h = control_button_wh

col_cover_spacer_w = top_panel_h - cover_wh - control_panel_h - GAP

seekbar_h = control_button_wh
seekbar_w = col_viz_w
viz_h = top_panel_h - GAP - seekbar_h - GAP - font_h

volume_h = viz_h

