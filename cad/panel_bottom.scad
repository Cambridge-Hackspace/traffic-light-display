/*
 * This is the panel for the bottom of the box.
 */

use <finger_joint_panel.scad>;
include <project_vars.scad>;

panel_width = box_width + wire_grace;
panel_depth = box_depth + wire_grace;
bolt_dia = 4;
foot_placement_ratio = 0.15;

$fn=100;

difference() {
  panel(
      panel_width,         // total inner width
      panel_depth,         // total inner depth
      tooth_sz, tooth_sz,  // teeth are square
      false                // top and bottom have negative corners
  );

  // 4x holes for feet
  translate([foot_placement_ratio * panel_width, foot_placement_ratio * panel_depth]) {
    circle(d = bolt_dia);
  }
  translate([foot_placement_ratio * panel_width, (1 - foot_placement_ratio) * panel_depth]) {
    circle(d = bolt_dia);
  }
  translate([(1 - foot_placement_ratio) * panel_width, foot_placement_ratio * panel_depth]) {
    circle(d = bolt_dia);
  }
  translate([(1 - foot_placement_ratio) * panel_width, (1 - foot_placement_ratio) * panel_depth]) {
    circle(d = bolt_dia);
  }
  
  // 6x holes for psu placement
  translate([box_width / 2, box_height + 1.5 * bolt_dia]) {
    translate([-bolt_dia, 0]) {
      circle(d = bolt_dia);
    }
    translate([bolt_dia, 0]) {
      circle(d = bolt_dia);
    }
  }
  translate([box_width + 1.5 * bolt_dia, 0]) {
    translate([0, box_height / 3]) {
      translate([0, -bolt_dia]) {
        circle(d = bolt_dia);
      }
      translate([0, bolt_dia]) {
        circle(d = bolt_dia);
      }
    }
    translate([0, 2 * box_height / 3]) {
      translate([0, -bolt_dia]) {
        circle(d = bolt_dia);
      }
      translate([0, bolt_dia]) {
        circle(d = bolt_dia);
      }
    }
  }
}
