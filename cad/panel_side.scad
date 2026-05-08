/*
 * This is the panel for the left and right sides of the box.
 * The left side is flush with the power supply.
 */

use <finger_joint_panel.scad>;
include <project_vars.scad>;

difference() {
  panel(
      box_depth + wire_grace,  // total inner depth
      box_height,              // box height
      tooth_sz, tooth_sz,      // teeth are square
      true                     // top and bottom have negative corners
  );
  
  logo_sz = box_height < box_depth + wire_grace ? box_height : box_depth + wire_grace; 
  translate([(box_depth + wire_grace) / 2, box_height / 2]) {
    import("chack_logo.svg", dpi=40, center=true, $fn=500);
  }
}