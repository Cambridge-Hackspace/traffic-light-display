// panel length and width exclude teeth

module _teeth(tooth_width, tooth_depth, tooth_count, tooth_offset, run_length) {
  for(i = [tooth_offset : tooth_width * 2 : run_length - tooth_width]) {
    translate([i, -tooth_depth]) {
      square([tooth_width, tooth_depth]);
    }
  }
}

module teeth(tooth_width, tooth_depth, run_length, invert_teeth=false) {
  tooth_count = floor(run_length / (2 * tooth_width) - 1);
  tooth_offset = (run_length - (tooth_count * 2 * tooth_width) - tooth_width) / 2;
  
  if(invert_teeth) {
    difference() {
      translate([0, -tooth_depth]) {
        square([run_length, tooth_depth]);
      }
      _teeth(tooth_width, tooth_depth, tooth_count, tooth_offset, run_length);
    }
  } else {
    _teeth(tooth_width, tooth_depth, tooth_count, tooth_offset, run_length);
  }
}

module panel(panel_length, panel_width, tooth_width, tooth_depth, invert_teeth=false, alternate_inversion=false) {

 tooth_count = floor(panel_width / (2 * tooth_width) - 1);
 tooth_offset = (panel_width - (tooth_count * 2 * tooth_width) - tooth_width) / 2;
 
 square([panel_length, panel_width]);
 transforms = ([
     [panel_length, 0, 0, 0, invert_teeth],
     [panel_length, 0, 0, panel_width + tooth_depth, invert_teeth],
     [panel_width, 90, -tooth_depth, 0, invert_teeth != alternate_inversion],
     [panel_width, 90, panel_length, 0, invert_teeth != alternate_inversion]
 ]);
 for(i = [0 : 3]) {
   transform = transforms[i];
   translate([transform[2], transform[3]]) {
     rotate([0, 0, transform[1]]) {
       teeth(tooth_width, tooth_depth, transform[0], transform[4]);
     }
   }
 }
}

//panel(104, 104, 5, 5, false);