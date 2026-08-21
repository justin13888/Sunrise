# @sunrise/models

Shared data models for Sunrise: [TypeBox](https://github.com/sinclairzx81/typebox) schemas with compiled, reusable validators. Used by the API server to validate routine inputs (e.g. flexibility 0–100, duration 5–480 minutes) and by the app for shared types and presets.

## Contents

- **Schemas + types**: `routineSchema`/`Routine`, `durationSchema`, `timeWindowSchema`, `dependencySchema`, `routineTemplateSchema`, priority/energy/frequency/time-of-day unions, and more
- **Validators**: `createValidator(schema)` compiles a schema into `{ check, assert, errors }`; prebuilt validators include `routineValidator`, `durationValidator`, `timeWindowValidator`, `dependencyValidator`, `routineTemplateValidator`
- **Helpers**: `validateRoutine(value)` type guard and `assertRoutine(value)` (throws with a readable list of every validation problem)
- **Presets**: `DEFAULT_ROUTINE_CATEGORIES`, `PRECONFIGURED_ROUTINES`, `ROUTINE_TEMPLATES`, `QUICK_RESCHEDULE_OPTIONS`
