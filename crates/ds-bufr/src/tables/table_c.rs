//! This file is generated from BUFR_TableC_en.txt.

use super::TableCEntry;

pub static TABLE_C: [TableCEntry; 28] = [
    TableCEntry {
        xy: (1, None),
        operator_name: "Change data width",
        operation_definition: "Add (YYY-128) bits to the data width given for each data element in Table B, other than CCITT IA5 (character) data, code or flag tables.",
    },
    TableCEntry {
        xy: (2, None),
        operator_name: "Change scale",
        operation_definition: "Add YYY-128 to the scale for each data element in Table B, other than CCITT IA5 (character) data, code or flag tables.",
    },
    TableCEntry {
        xy: (3, None),
        operator_name: "Change reference values",
        operation_definition: "Subsequent element descriptors define new reference values for corresponding Table B entries. Each new reference value is represented by YYY bits in the Data section. Definition of new reference values is concluded by coding this operator with YYY = 255. Negative reference values shall be represented by a positive integer with the left-most bit (bit 1) set to 1.",
    },
    TableCEntry {
        xy: (4, None),
        operator_name: "Add associated field",
        operation_definition: "Precede each data element with YYY bits of information.  This operation associates a data field (e.g. quality control information) of YYY bits with each data element.",
    },
    TableCEntry {
        xy: (5, None),
        operator_name: "Signify character",
        operation_definition: "YYY characters (CCITT International Alphabet No. 5) are inserted as a data field of YYY x 8 bits in length.",
    },
    TableCEntry {
        xy: (6, None),
        operator_name: "Signify data width for the immediately following local descriptor",
        operation_definition: "YYY bits of data are described by the immediately following descriptor.",
    },
    TableCEntry {
        xy: (7, None),
        operator_name: "Increase scale, reference value and data width",
        operation_definition: "For Table B elements, which are not CCITT IA5 (character data), code tables, or flag tables:   1. Add YYY to the existing scale factor   2. Multiply the existing reference value by 10**YYY   3. Calculate ((10 x YYY) + 2) ÷ 3, disregard any fractional remainder and add the result to the existing bit width.",
    },
    TableCEntry {
        xy: (8, None),
        operator_name: "Change width of CCITT IA5 field",
        operation_definition: "YYY characters from CCITT International Alphabet No. 5 (representing YYY x 8 bits in length) replace the specified data width given for each CCITT IA5 element in Table B.",
    },
    TableCEntry {
        xy: (21, None),
        operator_name: "Data not present",
        operation_definition: "Data values present in Section 4 (Data section) corresponding to the following YYY descriptors shall be limited to data from Classes 01-09, and Class 31.",
    },
    TableCEntry {
        xy: (22, Some(0)),
        operator_name: "Quality information follows",
        operation_definition: "The values of Class 33 elements which follow relate to the data defined by the data present bit-map.",
    },
    TableCEntry {
        xy: (23, Some(0)),
        operator_name: "Substituted values operator",
        operation_definition: "The substituted values which follow relate to the data defined by the data present bit-map.",
    },
    TableCEntry {
        xy: (23, Some(255)),
        operator_name: "Substituted values marker operator",
        operation_definition: "This operator shall signify a data item containing a substituted value; the element descriptor for the substituted value is obtained by the application of the data present bit-map associated with the substituted values operator.",
    },
    TableCEntry {
        xy: (24, Some(0)),
        operator_name: "First-order statistical values follow",
        operation_definition: "The statistical values which follow relate to the data defined by the data present bit-map.",
    },
    TableCEntry {
        xy: (24, Some(255)),
        operator_name: "First-order statistical values marker operator",
        operation_definition: "This operator shall signify a data item containing a first-order statistical value of the type indicated by the preceding 0 08 023 element descriptor; the element descriptor to which the first-order statistic relates is obtained by the application of the data present bit-map associated with the first-order statistical values follow operator; first-order statistical values shall be represented as defined by this element descriptor.",
    },
    TableCEntry {
        xy: (25, Some(0)),
        operator_name: "Difference statistical values follow",
        operation_definition: "The statistical values which follow relate to the data defined by the data present bit-map.",
    },
    TableCEntry {
        xy: (25, Some(255)),
        operator_name: "Difference statistical values marker operator",
        operation_definition: "This operator shall signify a data item containing a difference statistical value of the type indicated by the preceding 0 08 024 element descriptor; the element descriptor to which the difference statistical value relates is obtained by the application of the data present bit-map associated with the difference statistical values follow operator; difference statistical values shall be represented as defined by this element descriptor, but with a reference value of -2**n and a data width of (n+1), where n is the data width given by the original descriptor. This special reference value allows the statistical difference values to be centred around zero.",
    },
    TableCEntry {
        xy: (32, Some(0)),
        operator_name: "Replaced/retained values follow",
        operation_definition: "The replaced/retained values which follow relate to the data defined by the data present bit-map.",
    },
    TableCEntry {
        xy: (32, Some(255)),
        operator_name: "Replaced/retained value marker operator",
        operation_definition: "This operator shall signify a data item containing the original of an element which has been replaced by a substituted value.  The element descriptor for the retained value is obtained by the application of the data present bit-map associated with the substituted values operator.",
    },
    TableCEntry {
        xy: (35, Some(0)),
        operator_name: "Cancel backward data reference",
        operation_definition: "This operator terminates all previously defined back-ward reference and cancels any previously defined data present bit-map; it causes the next data present bit-map to refer to the data descriptors which immediately precede the operator to which it relates.",
    },
    TableCEntry {
        xy: (36, Some(0)),
        operator_name: "Define data present bit-map",
        operation_definition: "This operator defines the data present bit-map which follows for possible re-use; only one data present bit-map may be defined between this operator and the cancel use defined data present bit-map operator.",
    },
    TableCEntry {
        xy: (37, Some(0)),
        operator_name: "Use defined data present bit-map",
        operation_definition: "This operator causes the defined data present bit-map to be used again.",
    },
    TableCEntry {
        xy: (37, Some(255)),
        operator_name: "Cancel use defined data present bit-map",
        operation_definition: "This operator cancels the re-use of the defined data present bit-map.",
    },
    TableCEntry {
        xy: (41, Some(0)),
        operator_name: "Define event",
        operation_definition: "This operator denotes the beginning of the definition of an event.",
    },
    TableCEntry {
        xy: (41, Some(255)),
        operator_name: "Cancel define event",
        operation_definition: "This operator denotes the conclusion of the event definition that was begun via the previous 2 41 000 operator.",
    },
    TableCEntry {
        xy: (42, Some(0)),
        operator_name: "Define conditioning event",
        operation_definition: "This operator denotes the beginning of the definition of a conditioning event.",
    },
    TableCEntry {
        xy: (42, Some(255)),
        operator_name: "Cancel define conditioning event",
        operation_definition: "This operator denotes the conclusion of the conditioning event definition that was begun via the previous 2 42 000 operator.",
    },
    TableCEntry {
        xy: (43, Some(0)),
        operator_name: "Categorical forecast values follow",
        operation_definition: "The values which follow are categorical forecast values.",
    },
    TableCEntry {
        xy: (43, Some(255)),
        operator_name: "Cancel categorical forecast values follow",
        operation_definition: "This operator denotes the conclusion of the definition of categorical forecast values that was begun via the previous 2 43 000 operator.",
    },
];
