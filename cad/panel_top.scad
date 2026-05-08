/*
 * This is the panel for the top of the box.
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

lid_width = box_width + wire_grace;
lid_depth = box_depth + wire_grace;
wire_spacing = box_vmargin + wire_dia; // additional space for wire harness

// number of slits we can fit between vertical margins
// when considering slit spacing and wire hole diameter/margins
slit_count = floor((lid_depth - 2 * box_vmargin - wire_spacing - slit_spacing) / (slit_height + slit_spacing) + 1);

// the margin at the beginning and end of slits
// in order to ensure that they're centered
slit_margin = (lid_depth - 2 * box_vmargin - wire_spacing - slit_count * slit_height - (slit_count - 1) * slit_spacing) / 2;

$fn=100;

difference() {
  panel(
      lid_width,          // total inner width
      lid_depth,          // total inner depthq
      tooth_sz, tooth_sz, // teeth are square
      false               // top and bottom have negative corners
  );
  translate([box_hmargin, box_vmargin + slit_margin]) {
    for(i = [0 : slit_count - 1]) {
      translate([0, i * (slit_height + slit_spacing)]) {
        slot(lid_width - 2 * box_hmargin, slit_height);
      }
    }
  }
  translate([lid_width / 2, lid_depth - wire_dia / 2 - box_vmargin - slit_margin / 2]) {
    circle(d = wire_dia);
  }
}
