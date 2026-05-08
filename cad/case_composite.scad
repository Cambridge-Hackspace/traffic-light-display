include <project_vars.scad>;

// Camera Settings
$vpt = [0, 0, 70];
$vpr = [55, 0, 25];
$vpd = 900;

module import_panel(name) {
  linear_extrude(tooth_sz) {
    import(name);
  }
}

explode = 35 * (1 - cos($t * 360)) / 2;
rotation = $t * -720;

rotate([0, 0, rotation]) {
  translate([-(box_width + wire_grace)/2, -(box_depth + wire_grace)/2, 0]) {

    translate([0, 0, -explode]) {
      import_panel("panel_bottom.dxf");
    }

    translate([0, box_depth + wire_grace + tooth_sz + explode, tooth_sz]) {
      rotate([90, 0, 0]) {
        import_panel("panel_front.dxf");
      }
    }

    translate([0, -explode, tooth_sz]) {
      rotate([90, 0, 0]) {
        import_panel("panel_back.dxf");
      }
    }

    translate([box_width + wire_grace + explode, 0, tooth_sz]) {
      rotate([90, 0, 90]) {
        linear_extrude(tooth_sz * 0.5) {
          square([box_depth + wire_grace, box_height]);
        }
        import_panel("panel_side.dxf");
      }
    }

    translate([-explode, box_depth + wire_grace, tooth_sz]) {
      rotate([-90, 180, 90]) {
        linear_extrude(tooth_sz * 0.5) {
          square([box_depth + wire_grace, box_height]);
        }
        import_panel("panel_side.dxf");
      }
    }

    translate([0, 0, box_height + tooth_sz + explode]) {
      import_panel("panel_top.dxf");
    }

  }
}
