/*
 * This is the panel for the back of the box.
 * This is where the PSU will be plugged in.
 */

use <finger_joint_panel.scad>;
include <project_vars.scad>;

module slot(length, height) {
  translate([0, height/2, 0]) {
    hull() {
      translate([height/2, 0]) {
        circle(d = height);
      }
      translate([length - height/2, 0]) {
        circle(d = height);
      }
    }
  }
};

panel_width = box_width + wire_grace;
panel_height = box_height;
switch_spacing = box_vmargin + rot_switch_dia; // additional space for wire harness

// number of slits we can fit between vertical margins
// when considering slit spacing and wire hole diameter/margins
slit_count = floor((panel_height - 2 * box_vmargin - switch_spacing - slit_spacing) / (slit_height + slit_spacing) + 1);

// the margin at the beginning and end of slits
// in order to ensure that they're centered
slit_margin = (panel_height - 2 * box_vmargin - switch_spacing - slit_count * slit_height - (slit_count - 1) * slit_spacing) / 2;

$fn=100;

difference() {
  panel(
      panel_width,        // total inner width
      panel_height,       // total inner height
      tooth_sz, tooth_sz, // teeth are square
      true                // top and bottom have negative corners
  );
  translate([psu_lip, psu_lip]) {
    square([box_width - 2 * psu_lip, box_height - 2 * psu_lip]);
  }
  translate([box_width, box_vmargin + slit_margin]) {
    for(i = [0 : slit_count - 1]) {
      translate([0, i * (slit_height + slit_spacing)]) {
        slot(wire_grace - psu_lip, slit_height);
      }
    }
  }
  translate([box_width - psu_lip/2 + wire_grace/2, panel_height - rot_switch_dia / 2 - box_vmargin - slit_margin / 2]) {
    circle(d = rot_switch_dia);
  }
}
