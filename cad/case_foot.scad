bolt_hole = 4;
foot_dia = 30;
$fn=100;

r_minor = (foot_dia - bolt_hole) / 4;
r_major = (bolt_hole / 2) + r_minor;

echo("r_major=", r_major);
echo("r_minor=", r_minor);

difference() {
  rotate_extrude(angle=360) {
    translate([r_major, 0, 0]) {
      circle(r=r_minor);
    }
  }
  translate([0, 0, -r_minor/2]) {
    cube([foot_dia, foot_dia, r_minor], center=true);
  } 
  translate([0, 0, r_minor - bolt_hole]) {
    cylinder(h=bolt_hole, r=bolt_hole);
  }
}

