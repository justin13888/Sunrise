import { Type, type Static } from '@sinclair/typebox';

// Enum schemas
export const priorityLevelSchema = Type.Union([
  Type.Literal('high'),
  Type.Literal('medium'),
  Type.Literal('low')
]);
export type PriorityLevel = Static<typeof priorityLevelSchema>;

export const energyLevelSchema = Type.Union([
  Type.Literal('high'),
  Type.Literal('medium'),
  Type.Literal('low')
]);
export type EnergyLevel = Static<typeof energyLevelSchema>;

// Routine category definition schema
export const routineCategoryDefinitionSchema = Type.Object({
  /** Unique identifier for the category (UUIDv4) */
  id: Type.String({ 
    format: 'uuid',
    examples: ['550e8400-e29b-41d4-a716-446655440000', 'f47ac10b-58cc-4372-a567-0e02b2c3d479']
  }),
  /** Human-readable name for the category */
  name: Type.String({ 
    minLength: 1, 
    maxLength: 50, 
    examples: ['Work & Professional', 'Personal Care', 'Health & Fitness'] 
  }),
  /** Color for the category (hex code) */
  color: Type.String({ 
    pattern: '^#[0-9a-fA-F]{6}$',
    examples: ['#3B82F6', '#EF4444', '#10B981', '#F59E0B']
  })
});
export type RoutineCategoryDefinition = Static<typeof routineCategoryDefinitionSchema>;

// For backward compatibility and simpler referencing in routines
export const routineCategorySchema = Type.String({ 
  format: 'uuid',
  examples: ['550e8400-e29b-41d4-a716-446655440000']
});
export type RoutineCategory = Static<typeof routineCategorySchema>;

export const frequencySchema = Type.Union([
  Type.Literal('daily'),
  Type.Literal('weekly'),
  Type.Literal('weekdays'),
  Type.Literal('weekends'),
  Type.Literal('custom')
]);
export type Frequency = Static<typeof frequencySchema>;

export const timeOfDaySchema = Type.Union([
  Type.Literal('early_morning'),
  Type.Literal('morning'),
  Type.Literal('late_morning'),
  Type.Literal('midday'),
  Type.Literal('afternoon'),
  Type.Literal('evening'),
  Type.Literal('night'),
  Type.Literal('flexible')
]);
export type TimeOfDay = Static<typeof timeOfDaySchema>;

export const conflictResolutionSchema = Type.Union([
  Type.Literal('skip'),
  Type.Literal('reschedule'),
  Type.Literal('compress'),
  Type.Literal('override')
]);
export type ConflictResolution = Static<typeof conflictResolutionSchema>;

// Time duration schema
export const durationSchema = Type.Object({
  /** Duration in minutes */
  minutes: Type.Number({ minimum: 5, maximum: 480, examples: [30, 60, 120] }),
  /** Whether the duration can be adjusted */
  flexible: Type.Boolean({ default: false }),
  /** Minimum duration if flexible */
  min_duration: Type.Optional(Type.Number({ minimum: 5, examples: [15, 30] })),
  /** Maximum duration if flexible */
  max_duration: Type.Optional(Type.Number({ maximum: 480, examples: [90, 180] }))
});
export type Duration = Static<typeof durationSchema>;

// Time window schema
export const timeWindowSchema = Type.Object({
  /** Start hour (0-23) */
  start_hour: Type.Number({ minimum: 0, maximum: 23, examples: [9, 14] }),
  /** Start minute (0-59) */
  start_minute: Type.Number({ minimum: 0, maximum: 59, default: 0 }),
  /** End hour (0-23) */
  end_hour: Type.Number({ minimum: 0, maximum: 23, examples: [17, 22] }),
  /** End minute (0-59) */
  end_minute: Type.Number({ minimum: 0, maximum: 59, default: 0 })
});
export type TimeWindow = Static<typeof timeWindowSchema>;

// Dependency schema
export const dependencySchema = Type.Object({
  /** ID of the routine this depends on */
  routine_id: Type.String({ examples: ['wake_up', 'morning_exercise'] }),
  /** Type of dependency relationship */
  relationship: Type.Union([
    Type.Literal('before'),
    Type.Literal('after'),
    Type.Literal('same_time_block')
  ]),
  /** Buffer time in minutes between routines */
  buffer_minutes: Type.Optional(Type.Number({ minimum: 0, default: 0, examples: [5, 15, 30] }))
});
export type Dependency = Static<typeof dependencySchema>;

// Main routine schema
export const routineSchema = Type.Object({
  /** Unique identifier for the routine */
  id: Type.String({ examples: ['morning_exercise', 'deep_work_block'] }),
  /** Display name for the routine */
  name: Type.String({ minLength: 1, maxLength: 100, examples: ['Morning Exercise', 'Deep Work Block'] }),
  /** Optional description */
  description: Type.Optional(Type.String({ maxLength: 500, examples: ['Energizing physical activity to start the day'] })),
  
  // Core attributes
  /** How long the routine takes */
  duration: durationSchema,
  /** How important this routine is */
  priority: priorityLevelSchema,
  /** How flexible the scheduling is (0 = rigid, 10 = very flexible) */
  flexibility: Type.Number({ minimum: 0, maximum: 10, default: 5, examples: [2, 5, 8] }),
  /** Energy level required to perform this routine */
  energy_level_required: energyLevelSchema,
  /** Category of the routine */
  category: routineCategorySchema,
  
  // Scheduling
  /** How often this routine occurs */
  frequency: frequencySchema,
  /** Preferred times of day for this routine */
  time_preferences: Type.Array(timeOfDaySchema, { examples: [['morning', 'late_morning']] }),
  /** Time windows when this routine can be scheduled */
  availability_windows: Type.Array(timeWindowSchema),
  /** Other routines this depends on */
  dependencies: Type.Array(dependencySchema, { default: [] }),
  
  // Constraints
  /** Minimum gap in minutes between similar routines */
  minimum_gap_minutes: Type.Number({ minimum: 0, default: 0, examples: [0, 15, 30] }),
  /** Buffer time to add before/after this routine */
  buffer_time_minutes: Type.Number({ minimum: 0, default: 5, examples: [5, 10, 15] }),
  /** How to handle scheduling conflicts */
  conflict_resolution: conflictResolutionSchema,
  
  // Grouping preferences
  /** Whether this routine can be batched with others */
  can_be_grouped: Type.Boolean({ default: true }),
  /** Preferred number of similar routines to batch together */
  preferred_batch_size: Type.Optional(Type.Number({ minimum: 1, maximum: 10, examples: [2, 3] })),
  
  // Status
  /** Whether this routine is currently active */
  enabled: Type.Boolean({ default: true }),
  /** Tags for filtering and organization */
  tags: Type.Array(Type.String(), { default: [], examples: [['morning', 'fitness'], ['work', 'focus']] })
});
export type Routine = Static<typeof routineSchema>;

// Template/preset schema
export const routineTemplateSchema = Type.Object({
  /** Unique identifier for the template */
  id: Type.String({ examples: ['remote_worker_standard', 'early_bird_focused'] }),
  /** Display name for the template */
  name: Type.String({ examples: ['Remote Worker - Standard', 'Early Bird - High Productivity'] }),
  /** Description of what this template is for */
  description: Type.String({ examples: ['Balanced routine for remote workers with flexible schedule'] }),
  /** List of routines in this template */
  routines: Type.Array(routineSchema),
  /** Types of users this template is designed for */
  target_user_type: Type.Array(Type.String(), { examples: [['remote_worker', 'early_bird']] })
});
export type RoutineTemplate = Static<typeof routineTemplateSchema>;

// Manual control schemas
export const quickRescheduleOptionSchema = Type.Object({
  /** Display label for the option */
  label: Type.String({ examples: ['Delay 30 minutes', 'Move to tomorrow'] }),
  /** Action to perform */
  action: Type.Union([
    Type.Literal('delay_30min'),
    Type.Literal('delay_1hour'),
    Type.Literal('move_to_tomorrow'),
    Type.Literal('move_to_next_available'),
    Type.Literal('skip_today')
  ])
});
export type QuickRescheduleOption = Static<typeof quickRescheduleOptionSchema>;

export const manualOverrideSchema = Type.Object({
  /** ID of the routine to override */
  routine_id: Type.String({ examples: ['morning_exercise', 'deep_work_block'] }),
  /** Type of override to apply */
  override_type: Type.Union([
    Type.Literal('disable_temporarily'),
    Type.Literal('reschedule'),
    Type.Literal('modify_duration'),
    Type.Literal('change_priority')
  ]),
  /** New scheduled time (ISO datetime) */
  new_time: Type.Optional(Type.String({ examples: ['2025-05-28T09:00:00Z'] })),
  /** New duration in minutes */
  new_duration_minutes: Type.Optional(Type.Number({ examples: [45, 90] })),
  /** New priority level */
  new_priority: Type.Optional(priorityLevelSchema),
  /** When this override expires (ISO datetime) */
  expires_at: Type.Optional(Type.String({ examples: ['2025-05-29T00:00:00Z'] }))
});
export type ManualOverride = Static<typeof manualOverrideSchema>;

// Predefined routine categories
export const DEFAULT_ROUTINE_CATEGORIES: RoutineCategoryDefinition[] = [
  {
    id: "550e8400-e29b-41d4-a716-446655440000",
    name: "Work & Professional",
    color: "#3B82F6"
  },
  {
    id: "f47ac10b-58cc-4372-a567-0e02b2c3d479", 
    name: "Personal Care",
    color: "#10B981"
  },
  {
    id: "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
    name: "Health & Fitness", 
    color: "#EF4444"
  },
  {
    id: "6ba7b811-9dad-11d1-80b4-00c04fd430c8",
    name: "Social & Relationships",
    color: "#8B5CF6"
  },
  {
    id: "6ba7b812-9dad-11d1-80b4-00c04fd430c8",
    name: "Household & Maintenance",
    color: "#F59E0B"
  },
  {
    id: "6ba7b813-9dad-11d1-80b4-00c04fd430c8", 
    name: "Learning & Development",
    color: "#06B6D4"
  },
  {
    id: "6ba7b814-9dad-11d1-80b4-00c04fd430c8",
    name: "Creative & Hobbies",
    color: "#EC4899"
  }
];

// Preconfigured routine examples
export const PRECONFIGURED_ROUTINES: Routine[] = [
  // Morning Routines
  {
    id: "wake_up",
    name: "Wake up",
    description: "Natural wake up time with gentle transition",
    duration: { minutes: 15, flexible: true, min_duration: 10, max_duration: 30 },
    priority: "high",
    flexibility: 2,
    energy_level_required: "low",
    category: "f47ac10b-58cc-4372-a567-0e02b2c3d479", // Personal Care
    frequency: "daily",
    time_preferences: ["early_morning", "morning"],
    availability_windows: [
      { start_hour: 5, start_minute: 0, end_hour: 9, end_minute: 0 }
    ],
    dependencies: [],
    minimum_gap_minutes: 0,
    buffer_time_minutes: 0,
    conflict_resolution: "reschedule",
    can_be_grouped: false,
    enabled: true,
    tags: ["morning", "essential"]
  },
  {
    id: "morning_mindfulness",
    name: "Morning mindfulness/meditation",
    description: "Start the day with mindful intention setting",
    duration: { minutes: 10, flexible: true, min_duration: 5, max_duration: 20 },
    priority: "medium",
    flexibility: 6,
    energy_level_required: "low",
    category: "6ba7b810-9dad-11d1-80b4-00c04fd430c8", // Health & Fitness
    frequency: "daily",
    time_preferences: ["morning"],
    availability_windows: [
      { start_hour: 6, start_minute: 0, end_hour: 10, end_minute: 0 }
    ],
    dependencies: [
      { routine_id: "wake_up", relationship: "after", buffer_minutes: 5 }
    ],
    minimum_gap_minutes: 0,
    buffer_time_minutes: 5,
    conflict_resolution: "compress",
    can_be_grouped: false,
    enabled: true,
    tags: ["morning", "wellness", "mindfulness"]
  },
  {
    id: "morning_exercise",
    name: "Morning exercise/workout",
    description: "Energizing physical activity to start the day",
    duration: { minutes: 45, flexible: true, min_duration: 20, max_duration: 90 },
    priority: "high",
    flexibility: 4,
    energy_level_required: "medium",
    category: "6ba7b810-9dad-11d1-80b4-00c04fd430c8", // Health & Fitness
    frequency: "daily",
    time_preferences: ["morning", "late_morning"],
    availability_windows: [
      { start_hour: 6, start_minute: 0, end_hour: 11, end_minute: 0 }
    ],
    dependencies: [],
    minimum_gap_minutes: 30,
    buffer_time_minutes: 10,
    conflict_resolution: "reschedule",
    can_be_grouped: false,
    enabled: true,
    tags: ["morning", "fitness", "energy"]
  },

  // Work Focus Periods
  {
    id: "deep_work_morning",
    name: "Morning deep work block",
    description: "Focused, uninterrupted work on high-priority tasks",
    duration: { minutes: 120, flexible: true, min_duration: 60, max_duration: 180 },
    priority: "high",
    flexibility: 3,
    energy_level_required: "high",
    category: "550e8400-e29b-41d4-a716-446655440000", // Work & Professional
    frequency: "weekdays",
    time_preferences: ["morning", "late_morning"],
    availability_windows: [
      { start_hour: 8, start_minute: 0, end_hour: 12, end_minute: 0 }
    ],
    dependencies: [],
    minimum_gap_minutes: 15,
    buffer_time_minutes: 10,
    conflict_resolution: "override",
    can_be_grouped: false,
    enabled: true,
    tags: ["work", "focus", "high-priority"]
  },
  {
    id: "afternoon_focus",
    name: "Afternoon focus session",
    description: "Dedicated time for concentrated work after lunch",
    duration: { minutes: 90, flexible: true, min_duration: 45, max_duration: 120 },
    priority: "high",
    flexibility: 5,
    energy_level_required: "medium",
    category: "550e8400-e29b-41d4-a716-446655440000", // Work & Professional
    frequency: "weekdays",
    time_preferences: ["afternoon"],
    availability_windows: [
      { start_hour: 13, start_minute: 0, end_hour: 17, end_minute: 0 }
    ],
    dependencies: [],
    minimum_gap_minutes: 15,
    buffer_time_minutes: 10,
    conflict_resolution: "reschedule",
    can_be_grouped: false,
    enabled: true,
    tags: ["work", "focus", "afternoon"]
  },
  {
    id: "email_processing",
    name: "Email processing",
    description: "Dedicated time for email management and responses",
    duration: { minutes: 30, flexible: true, min_duration: 15, max_duration: 60 },
    priority: "medium",
    flexibility: 8,
    energy_level_required: "low",
    category: "550e8400-e29b-41d4-a716-446655440000", // Work & Professional
    frequency: "daily",
    time_preferences: ["morning", "afternoon", "flexible"],
    availability_windows: [
      { start_hour: 9, start_minute: 0, end_hour: 11, end_minute: 0 },
      { start_hour: 14, start_minute: 0, end_hour: 16, end_minute: 0 }
    ],
    dependencies: [],
    minimum_gap_minutes: 0,
    buffer_time_minutes: 5,
    conflict_resolution: "compress",
    can_be_grouped: true,
    preferred_batch_size: 2,
    enabled: true,
    tags: ["work", "communication", "admin"]
  },

  // Breaks & Transitions
  {
    id: "lunch_break",
    name: "Lunch break",
    description: "Midday meal and mental reset",
    duration: { minutes: 60, flexible: true, min_duration: 30, max_duration: 90 },
    priority: "high",
    flexibility: 4,
    energy_level_required: "low",
    category: "f47ac10b-58cc-4372-a567-0e02b2c3d479", // Personal Care
    frequency: "daily",
    time_preferences: ["midday"],
    availability_windows: [
      { start_hour: 11, start_minute: 30, end_hour: 14, end_minute: 30 }
    ],
    dependencies: [],
    minimum_gap_minutes: 0,
    buffer_time_minutes: 5,
    conflict_resolution: "override",
    can_be_grouped: false,
    enabled: true,
    tags: ["break", "meal", "essential"]
  },
  {
    id: "afternoon_break",
    name: "Afternoon energy boost",
    description: "Short break to recharge energy levels",
    duration: { minutes: 15, flexible: true, min_duration: 10, max_duration: 30 },
    priority: "medium",
    flexibility: 7,
    energy_level_required: "low",
    category: "f47ac10b-58cc-4372-a567-0e02b2c3d479", // Personal Care
    frequency: "weekdays",
    time_preferences: ["afternoon"],
    availability_windows: [
      { start_hour: 14, start_minute: 0, end_hour: 16, end_minute: 30 }
    ],
    dependencies: [],
    minimum_gap_minutes: 0,
    buffer_time_minutes: 5,
    conflict_resolution: "skip",
    can_be_grouped: true,
    enabled: true,
    tags: ["break", "energy", "wellness"]
  },

  // Evening Routines
  {
    id: "workday_shutdown",
    name: "End-of-workday shutdown",
    description: "Review day, plan tomorrow, and transition from work",
    duration: { minutes: 20, flexible: true, min_duration: 10, max_duration: 30 },
    priority: "high",
    flexibility: 3,
    energy_level_required: "medium",
    category: "550e8400-e29b-41d4-a716-446655440000", // Work & Professional
    frequency: "weekdays",
    time_preferences: ["evening"],
    availability_windows: [
      { start_hour: 16, start_minute: 0, end_hour: 19, end_minute: 0 }
    ],
    dependencies: [],
    minimum_gap_minutes: 0,
    buffer_time_minutes: 5,
    conflict_resolution: "reschedule",
    can_be_grouped: false,
    enabled: true,
    tags: ["work", "transition", "planning"]
  },
  {
    id: "wind_down",
    name: "Wind down routine",
    description: "Relaxing activities to prepare for sleep",
    duration: { minutes: 45, flexible: true, min_duration: 30, max_duration: 90 },
    priority: "high",
    flexibility: 5,
    energy_level_required: "low",
    category: "f47ac10b-58cc-4372-a567-0e02b2c3d479", // Personal Care
    frequency: "daily",
    time_preferences: ["evening", "night"],
    availability_windows: [
      { start_hour: 20, start_minute: 0, end_hour: 23, end_minute: 0 }
    ],
    dependencies: [],
    minimum_gap_minutes: 0,
    buffer_time_minutes: 10,
    conflict_resolution: "compress",
    can_be_grouped: false,
    enabled: true,
    tags: ["evening", "relaxation", "sleep-prep"]
  },

  // Weekly/Periodic
  {
    id: "weekly_planning",
    name: "Weekly planning session",
    description: "Review past week and plan upcoming week",
    duration: { minutes: 60, flexible: true, min_duration: 30, max_duration: 120 },
    priority: "high",
    flexibility: 6,
    energy_level_required: "high",
    category: "550e8400-e29b-41d4-a716-446655440000", // Work & Professional
    frequency: "weekly",
    time_preferences: ["morning", "afternoon"],
    availability_windows: [
      { start_hour: 9, start_minute: 0, end_hour: 17, end_minute: 0 }
    ],
    dependencies: [],
    minimum_gap_minutes: 0,
    buffer_time_minutes: 15,
    conflict_resolution: "reschedule",
    can_be_grouped: false,
    enabled: true,
    tags: ["planning", "weekly", "review"]
  },
  {
    id: "learning_time",
    name: "Learning/skill development",
    description: "Dedicated time for personal or professional learning",
    duration: { minutes: 60, flexible: true, min_duration: 30, max_duration: 120 },
    priority: "medium",
    flexibility: 8,
    energy_level_required: "medium",
    category: "6ba7b813-9dad-11d1-80b4-00c04fd430c8", // Learning & Development
    frequency: "daily",
    time_preferences: ["morning", "evening", "flexible"],
    availability_windows: [
      { start_hour: 7, start_minute: 0, end_hour: 9, end_minute: 0 },
      { start_hour: 19, start_minute: 0, end_hour: 22, end_minute: 0 }
    ],
    dependencies: [],
    minimum_gap_minutes: 0,
    buffer_time_minutes: 10,
    conflict_resolution: "reschedule",
    can_be_grouped: true,
    preferred_batch_size: 3,
    enabled: true,
    tags: ["learning", "development", "growth"]
  }
];

// Preconfigured templates for different user types
export const ROUTINE_TEMPLATES: RoutineTemplate[] = [
  {
    id: "remote_worker_standard",
    name: "Remote Worker - Standard",
    description: "Balanced routine for remote workers with flexible schedule",
    target_user_type: ["remote_worker", "flexible_schedule"],
    routines: PRECONFIGURED_ROUTINES.filter(r => 
      !r.tags.includes("commute") && r.frequency !== "weekends"
    )
  },
  {
    id: "early_bird_focused",
    name: "Early Bird - High Productivity",
    description: "Optimized for early risers who prefer morning productivity",
    target_user_type: ["early_bird", "morning_person", "high_productivity"],
    routines: PRECONFIGURED_ROUTINES.filter(r => 
      r.time_preferences.some(t => ["early_morning", "morning"].includes(t))
    )
  },
  {
    id: "fitness_focused",
    name: "Fitness Focused Professional",
    description: "Routine emphasizing health and fitness alongside work",
    target_user_type: ["fitness_focused", "health_conscious"],
    routines: PRECONFIGURED_ROUTINES.filter(r => 
      r.category === "6ba7b810-9dad-11d1-80b4-00c04fd430c8" || r.tags.includes("fitness") || r.priority === "high"
    )
  }
];

// Quick reschedule options
export const QUICK_RESCHEDULE_OPTIONS: QuickRescheduleOption[] = [
  { label: "Delay 30 minutes", action: "delay_30min" },
  { label: "Delay 1 hour", action: "delay_1hour" },
  { label: "Move to tomorrow", action: "move_to_tomorrow" },
  { label: "Find next available slot", action: "move_to_next_available" },
  { label: "Skip today", action: "skip_today" }
];
