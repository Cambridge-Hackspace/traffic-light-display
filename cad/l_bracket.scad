module l_bracket_face(bolt_hole, with_holes=false) {
  length = 3 * bolt_hole;
  width = 5 * bolt_hole;
  
  difference() {
    translate([(length-width/2)/2, 0]) {
      translate([(length-width/2)/2, 0]) {
        difference() {
          circle(d = width);
          translate([-width/4, 0]) {
            square([width/2, width], center = true);
          }
        }
      }
      square([length-width/2, width], center=true);
    }
    if(with_holes) {
      hull() {
        translate([bolt_hole, bolt_hole]) {
          circle(d=bolt_hole);
        }
        translate([bolt_hole * 1.5, bolt_hole]) {
          circle(d=bolt_hole);
        }
      }
      hull() {
        translate([bolt_hole, -bolt_hole]) {
          circle(d=bolt_hole);
        }
        translate([bolt_hole * 1.5, -bolt_hole]) {
          circle(d=bolt_hole);
        }
      }
    }
  }
}

module l_bracket() {
  bolt_hole = 4;
  thickness = 1;
  $fn=100;

  linear_extrude(thickness) {
    l_bracket_face(bolt_hole, true);
  }
  rotate([0, -90, 0]) {
    linear_extrude(thickness) {
      l_bracket_face(bolt_hole, false);
    }
  }
  difference() {
    translate([2 * thickness, thickness/2, 0]) {
      rotate([0, -90, 90]) {
        linear_extrude(thickness) {
          l_bracket_face(bolt_hole, false);
        }
      }
    }
    translate([-thickness, 0, 0]) {
      rotate([0,-90,0]) {
        cylinder(h=2*bolt_hole,r=3*bolt_hole);
      }
    }
  }
}

l_bracket();

